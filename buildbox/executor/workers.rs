//! Persistent workers: long-lived processes that each run many actions of
//! one kind, as Bazel's workers do. A worker is started with the action's
//! arguments but its flagfiles, and `--persistent_worker`; then for each
//! action it's sent a `WorkRequest` with the arguments its flagfiles hold,
//! and answers with a `WorkResponse`.
//!
//! An action runs in a worker when its platform has a `persistentWorkerKey`,
//! as Bazel sends for remote execution, and its last argument is a flagfile.
//! The key identifies the worker's tools; workers are shared by actions with
//! the same key, arguments and environment. Each worker keeps a directory of
//! its own, which each action's inputs are synced into, so the tools stay
//! in place.

use crate::inputs::{self, DirectoryCache, Staged};
use crate::{RunRequest, Runner, StartedProcess};
use cas::{Cas, Digest};
use executor::{Action, ExecuteError};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::Command;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The platform property that says an action runs in a worker, and which
/// workers can run it, as Bazel names it.
pub const WORKER_KEY_PROPERTY: &str = "persistentWorkerKey";

/// The platform property that says a worker speaks JSON rather than
/// protobuf, as `requires-worker-protocol = json` asks.
pub const WORKER_PROTOCOL_PROPERTY: &str = "persistentWorkerProtocol";

/// How many workers of one kind may run at once, as Bazel's default
/// `--worker_max_instances`.
const MAX_WORKERS_PER_KEY: usize = 4;

/// How long an idle worker is kept before it's stopped, so workers that
/// aren't being used don't hold memory.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often idle workers are looked for.
const REAP_INTERVAL: Duration = Duration::from_secs(30);

/// The language a worker speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Proto,
    Json,
}

/// Which workers can run an action: the same tools, arguments and
/// environment.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkerKey {
    tools: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    working_dir: PathBuf,
    protocol: Protocol,
}

/// How to run an action in a worker.
#[derive(Debug)]
pub struct WorkerSpec {
    key: WorkerKey,
    /// The arguments of the requests: the action's flagfile arguments.
    flagfiles: Vec<String>,
}

/// How an action runs in a worker, if it should: when its platform names
/// worker and its last argument is a flagfile, as Bazel requires.
pub fn worker_spec(action: &Action, working_dir: &Path) -> Option<WorkerSpec> {
    let property = |name: &str| action.platform.iter().find(|(property, _)| property == name).map(|(_, value)| value.as_str());
    let tools = property(WORKER_KEY_PROPERTY)?;
    let arguments = &action.command.arguments;
    if !arguments.last().is_some_and(|last| is_flagfile(last)) {
        return None;
    }
    let (flagfiles, mut args): (Vec<String>, Vec<String>) = arguments.iter().cloned().partition(|argument| is_flagfile(argument));
    args.push("--persistent_worker".to_string());
    let protocol = match property(WORKER_PROTOCOL_PROPERTY) {
        Some("json") => Protocol::Json,
        _ => Protocol::Proto,
    };
    let env = action.command.environment_variables.iter().map(|variable| (variable.name.clone(), variable.value.clone())).collect();
    let key = WorkerKey {
        tools: tools.to_string(),
        args,
        env,
        working_dir: working_dir.to_path_buf(),
        protocol,
    };
    Some(WorkerSpec { key, flagfiles })
}

/// Whether an argument names a flagfile: `@file`, or `--flagfile=file`.
fn is_flagfile(argument: &str) -> bool {
    (argument.starts_with('@') && !argument.starts_with("@@")) || argument.starts_with("--flagfile=") || argument.starts_with("-flagfile=")
}

/// The workers of an executor, by kind.
pub struct WorkerPool {
    root: PathBuf,
    state: Arc<Mutex<PoolState>>,
}

#[derive(Default)]
struct PoolState {
    idle: HashMap<WorkerKey, Vec<Worker>>,
    /// How many more workers of each kind may be busy.
    slots: HashMap<WorkerKey, Arc<Semaphore>>,
    next_id: u64,
}

impl WorkerPool {
    /// A pool of workers whose directories are made in `root`, stopping
    /// workers left idle.
    pub fn new(root: PathBuf) -> WorkerPool {
        let state = Arc::new(Mutex::new(PoolState::default()));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(reap_idle(Arc::downgrade(&state)));
        }
        WorkerPool { root, state }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A turn to use a worker of `key`'s kind, waiting while as many as may
    /// be are busy.
    async fn turn(&self, key: &WorkerKey) -> OwnedSemaphorePermit {
        let slots = self.lock().slots.entry(key.clone()).or_insert_with(|| Arc::new(Semaphore::new(MAX_WORKERS_PER_KEY))).clone();
        slots.acquire_owned().await.expect("worker slots are never closed")
    }

    fn take_idle(&self, key: &WorkerKey) -> Option<Worker> {
        let mut state = self.lock();
        let idle = state.idle.get_mut(key)?;
        while let Some(mut worker) = idle.pop() {
            if worker.process.process.is_running() {
                return Some(worker);
            }
        }
        None
    }

    fn put_back(&self, key: WorkerKey, mut worker: Worker) {
        worker.last_used = Instant::now();
        self.lock().idle.entry(key).or_default().push(worker);
    }

    /// A new directory for a worker.
    fn new_dir(&self) -> PathBuf {
        let mut state = self.lock();
        state.next_id += 1;
        self.root.join(format!("worker-{}-{}", std::process::id(), state.next_id))
    }
}

/// Stop workers idle for longer than [`IDLE_TIMEOUT`], until the pool is
/// gone.
async fn reap_idle(state: Weak<Mutex<PoolState>>) {
    loop {
        tokio::time::sleep(REAP_INTERVAL).await;
        let Some(state) = state.upgrade() else {
            return;
        };
        let stopped: Vec<Worker> = {
            let mut state = state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut stopped = Vec::new();
            for workers in state.idle.values_mut() {
                let (keep, stop): (Vec<Worker>, Vec<Worker>) = workers.drain(..).partition(|worker| worker.last_used.elapsed() < IDLE_TIMEOUT);
                *workers = keep;
                stopped.extend(stop);
            }
            stopped
        };
        drop(stopped);
    }
}

/// A running worker, and the directory it works in.
struct Worker {
    process: StartedProcess,
    dir: PathBuf,
    staged: Staged,
    /// The outputs of the last action, removed before the next one.
    outputs: Vec<PathBuf>,
    last_used: Instant,
}

impl Drop for Worker {
    fn drop(&mut self) {
        let dir = std::mem::take(&mut self.dir);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || std::fs::remove_dir_all(dir));
        }
    }
}

/// What a worker answered.
pub struct WorkResponse {
    pub exit_code: i32,
    pub output: String,
}

/// What running an action in a worker needs from its executor.
pub struct WorkerContext<'a> {
    pub cas: &'a dyn Cas,
    pub directories: &'a DirectoryCache,
    pub runner: &'a dyn Runner,
    pub pool: &'a WorkerPool,
}

/// Run an action in a worker of its kind: one that's idle, or a new one.
/// The action's outputs are left in the worker's directory, under
/// `working_dir`, for the caller to collect with `collect`, which is given
/// that directory.
pub async fn run<T, Collected>(
    context: &WorkerContext<'_>,
    action: &Action,
    spec: WorkerSpec,
    collect: impl FnOnce(PathBuf) -> Collected,
) -> Result<(WorkResponse, T), ExecuteError>
where
    Collected: std::future::Future<Output = Result<T, ExecuteError>>,
{
    let _turn = context.pool.turn(&spec.key).await;
    let mut worker = match context.pool.take_idle(&spec.key) {
        Some(worker) => worker,
        None => start(context, action, &spec).await?,
    };
    let ran = async {
        let response = request(context, &mut worker, action, &spec).await?;
        let collected = collect(worker.dir.join(&spec.key.working_dir)).await?;
        Ok((response, collected))
    }
    .await;
    if ran.is_ok() {
        context.pool.put_back(spec.key, worker);
    }
    ran
}

/// Start a worker for an action, its directory holding the action's
/// inputs, which include its tools.
async fn start(context: &WorkerContext<'_>, action: &Action, spec: &WorkerSpec) -> Result<Worker, ExecuteError> {
    let dir = context.pool.new_dir();
    let mut staged = Staged::default();
    tokio::fs::create_dir_all(dir.join(&spec.key.working_dir))
        .await
        .map_err(|error| ExecuteError::Internal(format!("failed to create {}: {error}", dir.display())))?;
    inputs::sync_inputs(context.cas, context.directories, &action.input_root, &dir, &mut staged).await?;
    let command = Command {
        arguments: spec.key.args.clone(),
        environment_variables: action.command.environment_variables.clone(),
        ..Default::default()
    };
    let request = RunRequest {
        command: &command,
        sandbox: &dir,
        working_dir: &spec.key.working_dir,
        timeout: None,
    };
    let process = context.runner.start(&request).await?;
    Ok(Worker {
        process,
        dir,
        staged,
        outputs: Vec::new(),
        last_used: Instant::now(),
    })
}

/// Send a worker an action's request, with its inputs in place, and wait
/// for its answer, for no longer than the action's timeout.
async fn request(context: &WorkerContext<'_>, worker: &mut Worker, action: &Action, spec: &WorkerSpec) -> Result<WorkResponse, ExecuteError> {
    let previous = std::mem::take(&mut worker.outputs);
    fsutil::run_blocking(move || remove_outputs(&previous)).await?;
    let digests = inputs::sync_inputs(context.cas, context.directories, &action.input_root, &worker.dir, &mut worker.staged).await?;
    let working_dir = worker.dir.join(&spec.key.working_dir);
    tokio::fs::create_dir_all(&working_dir)
        .await
        .map_err(|error| ExecuteError::Internal(format!("failed to create {}: {error}", working_dir.display())))?;
    crate::outputs::create_output_parents(&action.command, &working_dir).await?;
    worker.outputs = action.command.output_paths.iter().chain(&action.command.output_files).chain(&action.command.output_directories).map(|path| working_dir.join(path)).collect();
    let arguments = request_arguments(&spec.flagfiles, &working_dir).await?;
    let inputs = request_inputs(digests);
    let message = encode_request(spec.key.protocol, &arguments, &inputs);
    let exchange = async {
        let process = &mut worker.process.process;
        process.stdin.write_all(&message).await?;
        process.stdin.flush().await?;
        read_response(spec.key.protocol, &mut process.stdout).await
    };
    let answered = match action.timeout {
        Some(timeout) => tokio::time::timeout(timeout, exchange).await.map_err(|_| ExecuteError::Timeout(timeout))?,
        None => exchange.await,
    };
    answered.map_err(|error| {
        let stderr = worker.process.process.stderr();
        ExecuteError::Internal(format!(
            "Worker process quit or closed its stdin stream when we tried to send a WorkRequest: {error}\n---8<---8<--- Start of log ---8<---8<---\n{stderr}\n---8<---8<--- End of log ---8<---8<---"
        ))
    })
}

/// Remove an earlier action's outputs from a worker's directory.
fn remove_outputs(outputs: &[PathBuf]) -> Result<(), ExecuteError> {
    for output in outputs {
        let removed = match std::fs::symlink_metadata(output) {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(output),
            Ok(_) => std::fs::remove_file(output),
            Err(_) => Ok(()),
        };
        removed.map_err(|error| ExecuteError::Internal(format!("failed to remove {}: {error}", output.display())))?;
    }
    Ok(())
}

/// A request's arguments, as Bazel's are: the lines of each `@flagfile`,
/// and `--flagfile=` arguments as they are.
async fn request_arguments(flagfiles: &[String], working_dir: &Path) -> Result<Vec<String>, ExecuteError> {
    let mut arguments = Vec::new();
    for flagfile in flagfiles {
        let Some(path) = flagfile.strip_prefix('@') else {
            arguments.push(flagfile.clone());
            continue;
        };
        let path = working_dir.join(path);
        let contents = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| ExecuteError::InvalidAction(format!("failed to read the flagfile {}: {error}", path.display())))?;
        arguments.extend(contents.lines().map(str::to_string));
    }
    Ok(arguments)
}

/// The inputs a request lists, as Bazel lists them: those outside the
/// runfiles trees of its tools.
fn request_inputs(mut inputs: Vec<(String, Digest)>) -> Vec<(String, Digest)> {
    inputs.retain(|(path, _)| !path.split('/').any(|component| component.ends_with(".runfiles")));
    inputs
}

/// A `WorkRequest` with `arguments`, and inputs with their digests, as the
/// protocol writes it: a length-delimited protobuf, or a line of JSON.
fn encode_request(protocol: Protocol, arguments: &[String], inputs: &[(String, Digest)]) -> Vec<u8> {
    match protocol {
        Protocol::Json => {
            let inputs: Vec<serde_json::Value> = inputs
                .iter()
                .map(|(path, digest)| serde_json::json!({ "path": path, "digest": base64(digest.hash().as_bytes()) }))
                .collect();
            let mut line = serde_json::json!({ "arguments": arguments, "inputs": inputs }).to_string();
            line.push('\n');
            line.into_bytes()
        }
        Protocol::Proto => {
            let mut message = Vec::new();
            for argument in arguments {
                put_bytes_field(&mut message, 1, argument.as_bytes());
            }
            for (path, digest) in inputs {
                let mut input = Vec::new();
                put_bytes_field(&mut input, 1, path.as_bytes());
                put_bytes_field(&mut input, 2, digest.hash().as_bytes());
                put_bytes_field(&mut message, 2, &input);
            }
            let mut framed = Vec::with_capacity(message.len() + 5);
            put_varint(&mut framed, message.len() as u64);
            framed.extend(message);
            framed
        }
    }
}

/// Read a `WorkResponse`.
async fn read_response(protocol: Protocol, stdout: &mut tokio::process::ChildStdout) -> std::io::Result<WorkResponse> {
    match protocol {
        Protocol::Proto => {
            let length = read_varint(stdout).await?;
            let mut message = vec![0u8; usize::try_from(length).map_err(std::io::Error::other)?];
            stdout.read_exact(&mut message).await?;
            decode_response(&message).map_err(std::io::Error::other)
        }
        Protocol::Json => {
            let mut reader = BufReader::new(stdout);
            let mut text = String::new();
            loop {
                if reader.read_line(&mut text).await? == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the worker closed its stdout"));
                }
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                    return Ok(WorkResponse {
                        exit_code: json.get("exitCode").and_then(serde_json::Value::as_i64).unwrap_or(0) as i32,
                        output: json.get("output").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
                    });
                }
            }
        }
    }
}

/// A `WorkResponse` protobuf: its exit code, field 1, and output, field 2.
fn decode_response(mut bytes: &[u8]) -> Result<WorkResponse, String> {
    let mut response = WorkResponse { exit_code: 0, output: String::new() };
    while !bytes.is_empty() {
        let tag = take_varint(&mut bytes)?;
        match (tag >> 3, tag & 7) {
            (1, 0) => response.exit_code = take_varint(&mut bytes)? as i32,
            (2, 2) => {
                let length = take_varint(&mut bytes)? as usize;
                let text = bytes.get(..length).ok_or("truncated WorkResponse")?;
                response.output = String::from_utf8_lossy(text).into_owned();
                bytes = &bytes[length..];
            }
            (_, 0) => {
                take_varint(&mut bytes)?;
            }
            (_, 2) => {
                let length = take_varint(&mut bytes)? as usize;
                bytes = bytes.get(length..).ok_or("truncated WorkResponse")?;
            }
            (field, wire) => return Err(format!("unexpected field {field} of wire type {wire} in a WorkResponse")),
        }
    }
    Ok(response)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, field << 3 | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn take_varint(bytes: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or("truncated varint")?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("varint too long".to_string())
}

async fn read_varint(reader: &mut tokio::process::ChildStdout) -> std::io::Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = reader.read_u8().await?;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err(std::io::Error::other("varint too long"))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| value | u32::from(*byte) << (16 - 8 * index));
        for index in 0..4 {
            match index <= chunk.len() {
                true => out.push(ALPHABET[(value >> (18 - 6 * index) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_requests_as_bazel_does() {
        let digest = Digest::of_bytes(b"x");
        let request = encode_request(Protocol::Proto, &["-a".to_string()], &[("in".to_string(), digest.clone())]);
        let mut bytes = &request[..];
        let length = take_varint(&mut bytes).unwrap() as usize;
        assert_eq!(length, bytes.len());
        assert_eq!(&bytes[..4], &[10, 2, b'-', b'a']);
        let json = String::from_utf8(encode_request(Protocol::Json, &["-a".to_string()], &[])).unwrap();
        assert_eq!(json, "{\"arguments\":[\"-a\"],\"inputs\":[]}\n");
    }

    #[test]
    fn leaves_runfiles_out_of_requests() {
        let digest = Digest::empty();
        let inputs = ["src.txt", "bin/tool", "bin/tool.runfiles/_main/tool"].map(|path| (path.to_string(), digest.clone()));
        let paths: Vec<String> = request_inputs(inputs.to_vec()).into_iter().map(|(path, _)| path).collect();
        assert_eq!(paths, ["src.txt", "bin/tool"]);
    }

    #[test]
    fn reads_responses() {
        let mut message = Vec::new();
        put_varint(&mut message, 1 << 3);
        put_varint(&mut message, 2);
        put_bytes_field(&mut message, 2, b"warning");
        put_varint(&mut message, 3 << 3);
        put_varint(&mut message, 7);
        let response = decode_response(&message).unwrap();
        assert_eq!((response.exit_code, response.output.as_str()), (2, "warning"));
    }

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"a"), "YQ==");
    }
}
