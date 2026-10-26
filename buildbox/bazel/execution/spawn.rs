//! Running commands through the backend, with the action cache.

use crate::action_key::{ActionKey, ActionOutputs, Input, MISSING_INPUT, OutputFile, cas_error, description, input, progress, upload};
use crate::backend::Runners;
use bazel_progress::{ActionFailure, ActionOutput, ActionProgress, Command, Event, FailureReason};
use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::inputs::InputTree;
use crate::source::SourceTree;
use bazel_analysis::{Action, Artifact, ConfiguredTargetKey, Spawn};
use buffa::Message;
use intern::PathId;
use remote_execution_rust_connect::build::bazel::remote::execution::v2 as reapi;
use skyframe::{Ctx, SkyError};
use futures::StreamExt;
use persist::PersistedMap;
use skycodec::{Fingerprint, Fingerprinter};
use std::collections::{BTreeMap, BTreeSet, HashSet};
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
pub async fn run(ctx: &Ctx, backend: &Arc<Backend>, action_key: ActionKey, action: &Action, spawn: &Spawn) -> Result<ActionOutputs, SkyError> {
    let owner = action_key.owner;
    let span = tracing::info_span!("action_inputs", runfiles = action.runfiles.len());
    let generated_by = ready(ctx, action).instrument(span).await?;
    let shown = progress(ctx).map(|progress| progress.action_started(action_key.progress_id(), generated_by, description(action, owner)));
    let _turn = crate::jobs::job_turn(ctx).await?;
    let _staged = backend.stage().await?;
    if let Some(shown) = &shown {
        shown.running(backend.runners.run);
    }
    let inputs = match inputs(ctx, action).await {
        Ok(inputs) => inputs,
        Err(NotReady::MissingInputs(labels)) => {
            let result = reapi::ActionResult::default();
            return Err(report_failure(ctx, backend, action_key, action, FailureReason::MissingInputs(labels), &result).await);
        }
        Err(NotReady::Failed(error)) => return Err(error),
    };
    let (output_files, output_directories) = output_paths(action);
    let request = SpawnRequest {
        inputs,
        arguments: spawn.arguments.clone(),
        env: spawn.env.clone(),
        output_files,
        output_directories,
        cacheable: !spawn.execution_info.iter().any(|(key, _)| key == "no-cache"),
        timeout: None,
        description: description(action, owner),
    };
    let local = ctx.global::<LocalActionCache>().ok().filter(|_| request.cacheable);
    let key = local.as_ref().map(|_| local_key(&request));
    if let (Some(local), Some(key)) = (&local, &key) {
        let span = tracing::info_span!("local_cache_lookup");
        if let Some(result) = local.lookup(backend, key).instrument(span).await? {
            finish(shown, ACTION_CACHE_HIT);
            return checked_outputs(ctx, backend, action_key, action, &result).await;
        }
    }
    let command = Command {
        arguments: request.arguments.clone(),
        env: request.env.clone(),
        configuration: owner.config.get().digest.clone(),
        execution_platform: owner.config.get().host_platform.to_string(),
    };
    let spawned = execute(backend, request).await?;
    let result = &spawned.result;
    if result.exit_code != 0 {
        let reason = FailureReason::Exit {
            code: result.exit_code,
            command,
            sandboxed: backend.runners == Runners::LOCAL,
        };
        return Err(report_failure(ctx, backend, action_key, action, reason, result).await);
    }
    if let (Some(local), Some(key)) = (&local, key) {
        local.0.insert(key, &result.encode_to_vec());
    }
    finish(shown, backend.runners.name(spawned.cached));
    if !spawned.cached {
        report_output(ctx, backend, action_key, action, result).await;
    }
    checked_outputs(ctx, backend, action_key, action, result).await
}

/// The outputs of a result, or if it lacks any, the error the action fails
/// with, once the failure is reported.
async fn checked_outputs(ctx: &Ctx, backend: &Backend, key: ActionKey, action: &Action, result: &reapi::ActionResult) -> Result<ActionOutputs, SkyError> {
    match outputs(action, result) {
        Ok(outputs) => Ok(outputs),
        Err(Missing::Outputs(paths)) => Err(report_failure(ctx, backend, key, action, FailureReason::MissingOutputs(paths), result).await),
        Err(Missing::Invalid(error)) => Err(error),
    }
}

/// Tell whoever is watching what a successful action printed, if anything.
async fn report_output(ctx: &Ctx, backend: &Backend, key: ActionKey, action: &Action, result: &reapi::ActionResult) {
    let Some(progress) = progress(ctx) else {
        return;
    };
    let output = action_output(backend, description(action, key.owner), result).await;
    if !output.stdout.is_empty() || !output.stderr.is_empty() {
        progress.report(Event::ActionOutput(output));
    }
}

async fn action_output(backend: &Backend, description: String, result: &reapi::ActionResult) -> ActionOutput {
    ActionOutput {
        description,
        stdout: output_text(backend, &result.stdout_raw, result.stdout_digest.as_option()).await,
        stderr: output_text(backend, &result.stderr_raw, result.stderr_digest.as_option()).await,
    }
}

/// Tell whoever is watching why an action failed, and what it printed,
/// returning the error it fails with. The error is transient, so a later
/// build runs the action again, as Bazel does.
async fn report_failure(
    ctx: &Ctx,
    backend: &Backend,
    key: ActionKey,
    action: &Action,
    reason: FailureReason,
    result: &reapi::ActionResult,
) -> SkyError {
    let owner = key.owner;
    let location = bazel_loading::target_location(ctx, owner.label).await;
    let description = description(action, owner);
    let error = failure_error(location.as_deref(), &description, &reason);
    let failure = ActionFailure {
        error: error.clone(),
        location,
        output: action_output(backend, description.clone(), result).await,
        description,
        owner: owner_description(ctx, owner).await,
        mnemonic: action.mnemonic.to_string(),
        reason,
    };
    if let Some(progress) = progress(ctx) {
        progress.report(Event::ActionFailed(Arc::new(failure)));
    }
    SkyError::transient(error)
}

/// What an action is for, as Bazel describes it: its target, with its
/// kind, after the aspects whose action it is, if any.
async fn owner_description(ctx: &Ctx, owner: ConfiguredTargetKey) -> String {
    let kind = match ctx.compute(&owner).await {
        Ok(target) => target.kind.to_string(),
        Err(_) => String::new(),
    };
    let target = format!("{kind} rule target {}", owner.label);
    let aspects = &owner.config.get().aspects;
    if aspects.is_empty() {
        return target;
    }
    let aspects: Vec<String> = aspects.iter().map(|aspect| aspect.description()).collect();
    format!("aspects [{}] on {target}", aspects.join(", "))
}

/// The error an action fails with: one line saying where and how.
fn failure_error(location: Option<&str>, description: &str, reason: &FailureReason) -> String {
    let how = match reason {
        FailureReason::Exit { code, .. } => format!("(Exit {code})"),
        FailureReason::MissingOutputs(_) => "not all outputs were created or valid".to_string(),
        FailureReason::MissingInputs(labels) => format!("{} input file(s) do not exist", labels.len()),
    };
    match location {
        Some(location) => format!("{location}: {description} failed: {how}"),
        None => format!("{description} failed: {how}"),
    }
}

/// Report that an action finished `how`, if progress is being reported.
pub fn finish(shown: Option<ActionProgress>, how: &str) {
    if let Some(shown) = shown {
        shown.finish(how);
    }
}

/// What progress reports call finding an action's result in the
/// [`LocalActionCache`], as Bazel calls finding it in its own action cache.
pub const ACTION_CACHE_HIT: &str = "action cache hit";

/// Results of actions run before, kept across restarts by [`local_key`]:
/// cheaper to compute than an action's REAPI digest, which needs its input
/// tree, so a cached action is found without building one.
pub struct LocalActionCache(pub PersistedMap);

impl LocalActionCache {
    /// The cached result for `key`, if there is one and its outputs are
    /// still in the CAS.
    async fn lookup(&self, backend: &Backend, key: &Fingerprint) -> Result<Option<reapi::ActionResult>, SkyError> {
        let Some(bytes) = self.0.get(key) else {
            return Ok(None);
        };
        let Ok(result) = reapi::ActionResult::decode_from_slice(&bytes) else {
            return Ok(None);
        };
        let missing = backend.cas.find_missing(&output_digests(&result)).await.map_err(cas_error)?;
        Ok(missing.is_empty().then_some(result))
    }
}

/// Identifies what running a request produces: its command and every
/// input's path and digest.
fn local_key(request: &SpawnRequest) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(b"bb local action v1");
    let mut strings = |strings: &[String]| {
        fingerprinter.u64(strings.len() as u64);
        for string in strings {
            fingerprinter.u64(string.len() as u64).bytes(string.as_bytes());
        }
    };
    strings(&request.arguments);
    strings(&request.output_files);
    strings(&request.output_directories);
    for (name, value) in &request.env {
        fingerprinter.u64(name.len() as u64).bytes(name.as_bytes()).u64(value.len() as u64).bytes(value.as_bytes());
    }
    fingerprinter.u64(request.timeout.map_or(0, |timeout| timeout.as_millis() as u64));
    fingerprinter.u64(request.inputs.len() as u64);
    for (path, input) in &request.inputs {
        fingerprinter.u64(path.len() as u64).bytes(path.as_bytes());
        fingerprinter.bytes(input.digest.to_cas().hash().as_bytes()).u64(input.digest.to_cas().size_bytes() as u64);
        fingerprinter.u64(input.executable as u64 | (input.is_directory as u64) << 1);
    }
    fingerprinter.finish()
}

/// The digests of a result's output files and directory trees.
fn output_digests(result: &reapi::ActionResult) -> Vec<cas::Digest> {
    let file_digests = result.output_files.iter().filter_map(|file| file.digest.as_option());
    let tree_digests = result.output_directories.iter().filter_map(|dir| dir.tree_digest.as_option());
    file_digests.chain(tree_digests).filter_map(|digest| cas::Digest::from_proto(digest).ok()).collect()
}

/// Why an action's inputs aren't ready.
enum NotReady {
    /// These source files, by label, don't exist.
    MissingInputs(Vec<String>),
    Failed(SkyError),
}

/// Wait until the actions that generate the action's inputs have run,
/// holding only their keys, so that an action waiting on others holds no
/// list of its inputs. Returns their progress ids.
async fn ready(ctx: &Ctx, action: &Action) -> Result<Vec<u64>, SkyError> {
    let generating = generating_actions(ctx, action).await?;
    let results = futures::future::join_all(generating.iter().map(|key| ctx.compute(key))).await;
    for result in results {
        result?;
    }
    let mut ids: Vec<u64> = generating.iter().map(ActionKey::progress_id).collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The actions that generate an action's inputs.
async fn generating_actions(ctx: &Ctx, action: &Action) -> Result<BTreeSet<ActionKey>, SkyError> {
    let mut by_owner: BTreeMap<ConfiguredTargetKey, Vec<PathId>> = BTreeMap::new();
    action.for_each_input(|artifact| {
        if let Some(config) = artifact.config {
            let owner = ConfiguredTargetKey { label: artifact.owner, config };
            by_owner.entry(owner).or_default().push(artifact.exec_path);
        }
    });
    let owners = by_owner.into_iter().map(|(owner, paths)| owner_actions(ctx, owner, paths));
    let mut keys = BTreeSet::new();
    for result in futures::future::join_all(owners).await {
        keys.extend(result?);
    }
    Ok(keys)
}

/// The actions of `owner` that generate `paths`.
async fn owner_actions(ctx: &Ctx, owner: ConfiguredTargetKey, mut paths: Vec<PathId>) -> Result<BTreeSet<ActionKey>, SkyError> {
    paths.sort_unstable();
    paths.dedup();
    let target = ctx.compute(&owner).await?;
    let mut keys = BTreeSet::new();
    for path in paths {
        let Some((index, _)) = target.generating_action(path) else {
            return Err(SkyError::new(format!("{}: no action generates '{path}'", owner.label)));
        };
        keys.insert(ActionKey { owner, index });
    }
    Ok(keys)
}

/// Every input, by its path in the input tree: the action's inputs, and
/// the entries of its tools' runfiles trees.
async fn inputs(ctx: &Ctx, action: &Action) -> Result<BTreeMap<String, Input>, NotReady> {
    let unique = action.all_inputs();
    let results = futures::future::join_all(unique.iter().map(|artifact| input(ctx, *artifact))).await;
    let mut by_artifact: BTreeMap<Artifact, Input> = BTreeMap::new();
    let mut missing = Vec::new();
    for (artifact, result) in unique.into_iter().zip(results) {
        match result {
            Ok(input) => {
                by_artifact.insert(artifact, input);
            }
            Err(error) => match error.message().strip_prefix(MISSING_INPUT) {
                Some(label) => missing.push(label.trim_matches('\'').to_string()),
                None => return Err(NotReady::Failed(error)),
            },
        }
    }
    if !missing.is_empty() {
        return Err(NotReady::MissingInputs(missing));
    }
    let mut paths: BTreeMap<String, Input> = BTreeMap::new();
    action.inputs.for_each(|artifact| {
        paths.insert(artifact.exec_path.to_string(), by_artifact[&artifact].clone());
    });
    for tree in &action.runfiles {
        for (path, artifact) in &tree.entries {
            paths.insert(format!("{}/{path}", tree.dir), by_artifact[artifact].clone());
        }
    }
    Ok(paths)
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
        if let Some(source_tree) = &input.tree {
            add_source_tree(&mut tree, &mut blobs, path, source_tree)?;
            continue;
        }
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

/// Add a source directory at `path` in the input tree, file by file.
fn add_source_tree(tree: &mut InputTree, blobs: &mut Vec<(cas::Digest, Vec<u8>)>, path: &str, source_tree: &SourceTree) -> Result<(), SkyError> {
    let mut empty_directories: Vec<String> = source_tree.empty_directories.iter().map(|empty| format!("{path}/{empty}")).collect();
    if source_tree.files.is_empty() && empty_directories.is_empty() {
        empty_directories.push(path.to_string());
    }
    if !empty_directories.is_empty() {
        let empty = cas::Digest::of_bytes(&[]);
        blobs.push((empty.clone(), Vec::new()));
        for directory in &empty_directories {
            tree.add_directory(directory, BlobDigest::from_cas(&empty)).map_err(SkyError::new)?;
        }
    }
    for (file, digest, executable) in &source_tree.files {
        tree.add_file(&format!("{path}/{file}"), *digest, *executable).map_err(SkyError::new)?;
    }
    Ok(())
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
    let missing = backend.cas.find_missing(&output_digests(&result)).await.map_err(cas_error)?;
    Ok(missing.is_empty().then_some(result))
}

/// Why a result's outputs can't be used.
enum Missing {
    /// These declared outputs, by path from their root, weren't created.
    Outputs(Vec<String>),
    /// An output's digest isn't valid.
    Invalid(SkyError),
}

/// The outputs of a result, which must include every declared output.
fn outputs(action: &Action, result: &reapi::ActionResult) -> Result<ActionOutputs, Missing> {
    let mut files = Vec::with_capacity(action.outputs.len());
    let mut missing = Vec::new();
    for output in &action.outputs {
        let path = output.exec_path.to_string();
        let found = match output.is_directory {
            false => result.output_files.iter().find(|file| file.path == path).map(|file| (file.digest.as_option(), file.is_executable)),
            true => result.output_directories.iter().find(|dir| dir.path == path).map(|dir| (dir.tree_digest.as_option(), false)),
        };
        let Some((Some(digest), executable)) = found else {
            missing.push(root_relative(&path));
            continue;
        };
        let digest = cas::Digest::from_proto(digest).map_err(|error| Missing::Invalid(SkyError::new(format!("output '{path}': {error}"))))?;
        files.push(OutputFile {
            path: PathId::parse(&path).unwrap_or(output.exec_path),
            digest: BlobDigest::from_cas(&digest),
            executable,
            is_directory: output.is_directory,
        });
    }
    if !missing.is_empty() {
        return Err(Missing::Outputs(missing));
    }
    Ok(ActionOutputs { files })
}

/// An output's path from its root, as messages show it: without the
/// `bazel-out/<configuration>/bin/` it's under.
fn root_relative(path: &str) -> String {
    let parts: Vec<&str> = path.splitn(4, '/').collect();
    match parts.as_slice() {
        ["bazel-out", _, _, rest] => rest.to_string(),
        _ => path.to_string(),
    }
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
