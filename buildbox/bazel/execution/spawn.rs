//! Running commands through the backend, with the action cache.

use crate::action_key::{ActionOutputs, Input, OutputFile, cas_error, input, upload};
use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::inputs::InputTree;
use bazel_analysis::{Action, Artifact, ConfiguredTargetKey, Spawn};
use buffa::Message;
use intern::PathId;
use remote_execution_rust_connect::build::bazel::remote::execution::v2 as reapi;
use skyframe::{Ctx, SkyError};
use futures::StreamExt;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tracing::Instrument;

/// How much of an action's output an error message shows.
const MAX_OUTPUT_IN_ERROR: usize = 64 * 1024;

/// A command to run through the backend.
pub struct SpawnRequest {
    /// The files of the input tree, by path.
    pub inputs: BTreeMap<String, Input>,
    pub arguments: Vec<String>,
    /// Sorted by name.
    pub env: Vec<(String, String)>,
    pub output_files: Vec<String>,
    pub output_directories: Vec<String>,
    /// Whether the result may be taken from, and stored in, the action
    /// cache.
    pub cacheable: bool,
    pub timeout: Option<std::time::Duration>,
    /// What to call the command in messages.
    pub description: String,
}

/// What running a command produced.
pub struct SpawnResult {
    pub result: reapi::ActionResult,
    /// Whether the result came from the action cache.
    pub cached: bool,
}

/// Run a command, or reuse a cached successful result. A command that runs
/// and exits with a non-zero code still has a result.
pub async fn execute(backend: &Backend, request: SpawnRequest) -> Result<SpawnResult, SkyError> {
    let span = tracing::info_span!("input_tree", inputs = request.inputs.len());
    let tree = input_tree(backend, &request.inputs, &request.output_directories).instrument(span).await?;
    let command = command_proto(&request);
    let command_bytes = command.encode_to_vec();
    let command_digest = cas::Digest::of_bytes(&command_bytes);
    let proto = reapi::Action {
        command_digest: command_digest.to_proto().into(),
        input_root_digest: tree.root.to_proto().into(),
        do_not_cache: !request.cacheable,
        timeout: request.timeout.map(|timeout| buffa_types::google::protobuf::Duration {
            seconds: timeout.as_secs() as i64,
            ..Default::default()
        }).into(),
        ..Default::default()
    };
    let action_bytes = proto.encode_to_vec();
    let action_digest = cas::Digest::of_bytes(&action_bytes);

    let mut blobs = tree.blobs;
    blobs.push((command_digest, command_bytes));
    blobs.push((action_digest.clone(), action_bytes));
    let sources: Vec<&Input> = request.inputs.values().collect();
    let span = tracing::info_span!("upload_inputs", blobs = blobs.len(), inputs = sources.len());
    async {
        upload_blobs(backend, blobs).await?;
        upload(backend, &sources).await
    }
    .instrument(span)
    .await?;

    if request.cacheable {
        let span = tracing::info_span!("action_cache_lookup");
        if let Some(result) = cached(backend, &action_digest).instrument(span).await? {
            return Ok(SpawnResult { result, cached: true });
        }
    }
    let executable = executor::Action {
        digest: action_digest.clone(),
        command,
        input_root: tree.root,
        timeout: request.timeout,
        platform: Vec::new(),
    };
    tracing::info!("{}", request.description);
    let result = backend
        .executor
        .execute(&executable)
        .await
        .map_err(|error| SkyError::transient(format!("{}: {error}", request.description)))?;
    if request.cacheable && result.exit_code == 0 {
        upload_outputs(backend, &result).await?;
        backend
            .action_cache
            .put(&action_digest, &result)
            .await
            .map_err(|error| SkyError::transient(format!("action cache: {error}")))?;
    }
    Ok(SpawnResult { result, cached: false })
}

/// Run a spawn action, or reuse its cached result.
pub async fn run(ctx: &Ctx, backend: &Arc<Backend>, owner: ConfiguredTargetKey, action: &Action, spawn: &Spawn) -> Result<ActionOutputs, SkyError> {
    let (output_files, output_directories) = output_paths(action);
    let span = tracing::info_span!("action_inputs", inputs = action.inputs.len(), runfiles = action.runfiles.len());
    let request = SpawnRequest {
        inputs: inputs(ctx, action).instrument(span).await?,
        arguments: spawn.arguments.clone(),
        env: spawn.env.clone(),
        output_files,
        output_directories,
        cacheable: !spawn.execution_info.iter().any(|(key, _)| key == "no-cache"),
        timeout: None,
        description: progress(action, owner),
    };
    let spawned = execute(backend, request).await?;
    if spawned.result.exit_code != 0 {
        return Err(failed(backend, action, owner, &spawned.result).await);
    }
    outputs(action, &spawned.result)
}

fn progress(action: &Action, owner: ConfiguredTargetKey) -> String {
    match action.progress_message.is_empty() {
        true => format!("{} {}", action.mnemonic, owner.label),
        false => action.progress_message.clone(),
    }
}

/// Every input, by its path in the input tree: the action's inputs, and
/// the entries of its tools' runfiles trees.
async fn inputs(ctx: &Ctx, action: &Action) -> Result<BTreeMap<String, Input>, SkyError> {
    let mut paths: Vec<(String, Artifact)> = action.inputs.iter().map(|artifact| (artifact.exec_path.to_string(), *artifact)).collect();
    for tree in &action.runfiles {
        for (path, artifact) in &tree.entries {
            paths.push((format!("{}/{path}", tree.dir), *artifact));
        }
    }
    let mut unique: Vec<Artifact> = paths.iter().map(|(_, artifact)| *artifact).collect();
    unique.sort();
    unique.dedup();
    let digests = futures::future::try_join_all(unique.iter().map(|artifact| input(ctx, *artifact))).await?;
    let by_artifact: BTreeMap<Artifact, Input> = unique.into_iter().zip(digests).collect();
    Ok(paths.into_iter().map(|(path, artifact)| (path, by_artifact[&artifact].clone())).collect())
}

/// The input tree's `Directory` messages. Directories that are outputs of
/// other actions are spliced in from their `Tree`s. The action's output
/// directories are in it, empty, as Bazel gives them, since tools expect
/// them to exist and workers don't create them.
async fn input_tree(backend: &Backend, inputs: &BTreeMap<String, Input>, output_directories: &[String]) -> Result<crate::inputs::MerkleTree, SkyError> {
    let mut tree = InputTree::default();
    let mut blobs = Vec::new();
    if !output_directories.is_empty() {
        let empty = cas::Digest::of_bytes(&[]);
        blobs.push((empty.clone(), Vec::new()));
        for path in output_directories {
            tree.add_directory(path, BlobDigest::from_cas(&empty)).map_err(SkyError::new)?;
        }
    }
    for (path, input) in inputs {
        let added = match input.is_directory {
            false => tree.add_file(path, input.digest, input.executable),
            true => {
                let (root, directories) = tree_directories(backend, input.digest).await?;
                blobs.extend(directories);
                tree.add_directory(path, root)
            }
        };
        added.map_err(SkyError::new)?;
    }
    let mut merkle = tree.finish();
    merkle.blobs.extend(blobs);
    Ok(merkle)
}

/// The root of a `Tree` and every `Directory` in it, as blobs.
async fn tree_directories(backend: &Backend, tree: BlobDigest) -> Result<(BlobDigest, Vec<(cas::Digest, Vec<u8>)>), SkyError> {
    let bytes = backend.cas.read_bytes(&tree.to_cas()).await.map_err(cas_error)?;
    let tree = reapi::Tree::decode_from_slice(&bytes).map_err(|error| SkyError::new(format!("invalid Tree {tree:?}: {error}")))?;
    let mut blobs = Vec::new();
    let root = tree.root.as_option().cloned().unwrap_or_default();
    let root_bytes = root.encode_to_vec();
    let root_digest = cas::Digest::of_bytes(&root_bytes);
    blobs.push((root_digest.clone(), root_bytes));
    for child in &tree.children {
        let bytes = child.encode_to_vec();
        blobs.push((cas::Digest::of_bytes(&bytes), bytes));
    }
    Ok((BlobDigest::from_cas(&root_digest), blobs))
}

async fn upload_blobs(backend: &Backend, blobs: Vec<(cas::Digest, Vec<u8>)>) -> Result<(), SkyError> {
    let digests: Vec<cas::Digest> = blobs.iter().map(|(digest, _)| digest.clone()).collect();
    let missing: HashSet<cas::Digest> = backend.cas.find_missing(&digests).await.map_err(cas_error)?.into_iter().collect();
    let mut puts = Vec::with_capacity(missing.len());
    for (digest, bytes) in &blobs {
        if missing.contains(digest) {
            puts.push(backend.cas.put_bytes(bytes));
        }
    }
    let results: Vec<_> = futures::stream::iter(puts).buffer_unordered(crate::action_key::UPLOADS).collect().await;
    for result in results {
        result.map_err(cas_error)?;
    }
    Ok(())
}

/// Make sure a result's outputs are everywhere the CAS keeps blobs, such as
/// a remote cache in front of which actions run locally, before the result
/// is cached.
async fn upload_outputs(backend: &Backend, result: &reapi::ActionResult) -> Result<(), SkyError> {
    let file_digests = result.output_files.iter().filter_map(|file| file.digest.as_option());
    let tree_digests = result.output_directories.iter().filter_map(|dir| dir.tree_digest.as_option());
    let digests: Vec<cas::Digest> = file_digests.chain(tree_digests).filter_map(|digest| cas::Digest::from_proto(digest).ok()).collect();
    for digest in backend.cas.find_missing(&digests).await.map_err(cas_error)? {
        let data = backend.cas.read_bytes(&digest).await.map_err(cas_error)?;
        backend.cas.put_bytes(&data).await.map_err(cas_error)?;
    }
    Ok(())
}

/// The outputs of an action, as files and directories, sorted.
fn output_paths(action: &Action) -> (Vec<String>, Vec<String>) {
    let mut files: Vec<String> = Vec::new();
    let mut directories: Vec<String> = Vec::new();
    for output in &action.outputs {
        match output.is_directory {
            true => directories.push(output.exec_path.to_string()),
            false => files.push(output.exec_path.to_string()),
        }
    }
    files.sort();
    directories.sort();
    (files, directories)
}

fn command_proto(request: &SpawnRequest) -> reapi::Command {
    let environment_variables = request
        .env
        .iter()
        .map(|(name, value)| reapi::command::EnvironmentVariable {
            name: name.clone(),
            value: value.clone(),
            ..Default::default()
        })
        .collect();
    reapi::Command {
        arguments: request.arguments.clone(),
        environment_variables,
        output_files: request.output_files.clone(),
        output_directories: request.output_directories.clone(),
        ..Default::default()
    }
}

/// A cached successful result, if there is one and the outputs it names
/// are still in the CAS.
async fn cached(backend: &Backend, digest: &cas::Digest) -> Result<Option<reapi::ActionResult>, SkyError> {
    let result = backend
        .action_cache
        .get(digest)
        .await
        .map_err(|error| SkyError::transient(format!("action cache: {error}")))?;
    let Some(result) = result.filter(|result| result.exit_code == 0) else {
        return Ok(None);
    };
    let file_digests = result.output_files.iter().filter_map(|file| file.digest.as_option());
    let tree_digests = result.output_directories.iter().filter_map(|dir| dir.tree_digest.as_option());
    let digests: Vec<cas::Digest> = file_digests.chain(tree_digests).filter_map(|digest| cas::Digest::from_proto(digest).ok()).collect();
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    Ok(missing.is_empty().then_some(result))
}

/// The outputs of a result, which must include every declared output.
fn outputs(action: &Action, result: &reapi::ActionResult) -> Result<ActionOutputs, SkyError> {
    let mut files = Vec::with_capacity(action.outputs.len());
    for output in &action.outputs {
        let path = output.exec_path.to_string();
        let found = match output.is_directory {
            false => result.output_files.iter().find(|file| file.path == path).map(|file| (file.digest.as_option(), file.is_executable)),
            true => result.output_directories.iter().find(|dir| dir.path == path).map(|dir| (dir.tree_digest.as_option(), false)),
        };
        let Some((Some(digest), executable)) = found else {
            return Err(SkyError::new(format!("output '{path}' was not created")));
        };
        let digest = cas::Digest::from_proto(digest).map_err(|error| SkyError::new(format!("output '{path}': {error}")))?;
        files.push(OutputFile {
            path: PathId::parse(&path).unwrap_or(output.exec_path),
            digest: BlobDigest::from_cas(&digest),
            executable,
            is_directory: output.is_directory,
        });
    }
    Ok(ActionOutputs { files })
}

/// The error for an action that exited with a non-zero code, with what it
/// printed.
async fn failed(backend: &Backend, action: &Action, owner: ConfiguredTargetKey, result: &reapi::ActionResult) -> SkyError {
    let stdout = output_text(backend, &result.stdout_raw, result.stdout_digest.as_option()).await;
    let stderr = output_text(backend, &result.stderr_raw, result.stderr_digest.as_option()).await;
    let mut message = format!(
        "{}: {} failed: (Exit {}): {}",
        owner.label,
        progress(action, owner),
        result.exit_code,
        action.mnemonic
    );
    for text in [stdout, stderr] {
        if !text.is_empty() {
            message.push('\n');
            message.push_str(text.trim_end());
        }
    }
    SkyError::transient(message)
}

/// What a command printed to one of its streams, up to a limit.
pub async fn output_text(backend: &Backend, raw: &[u8], digest: Option<&reapi::Digest>) -> String {
    if !raw.is_empty() {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let Some(digest) = digest.and_then(|digest| cas::Digest::from_proto(digest).ok()) else {
        return String::new();
    };
    match backend.cas.read_bytes(&digest).await {
        Ok(bytes) => {
            let shown = &bytes[..bytes.len().min(MAX_OUTPUT_IN_ERROR)];
            String::from_utf8_lossy(shown).into_owned()
        }
        Err(_) => String::new(),
    }
}
