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
use std::collections::BTreeMap;
use std::sync::Arc;

/// How much of an action's output an error message shows.
const MAX_OUTPUT_IN_ERROR: usize = 64 * 1024;

/// Run a spawn action, or reuse its cached result.
pub async fn run(ctx: &Ctx, backend: &Arc<Backend>, owner: ConfiguredTargetKey, action: &Action, spawn: &Spawn) -> Result<ActionOutputs, SkyError> {
    let inputs = inputs(ctx, action).await?;
    let tree = input_tree(backend, &inputs).await?;
    let command = command(action, spawn);
    let command_bytes = command.encode_to_vec();
    let command_digest = cas::Digest::of_bytes(&command_bytes);
    let no_cache = spawn.execution_info.iter().any(|(key, _)| key == "no-cache");
    let proto = reapi::Action {
        command_digest: command_digest.to_proto().into(),
        input_root_digest: tree.root.to_proto().into(),
        do_not_cache: no_cache,
        ..Default::default()
    };
    let action_bytes = proto.encode_to_vec();
    let action_digest = cas::Digest::of_bytes(&action_bytes);

    let mut blobs = tree.blobs;
    blobs.push((command_digest, command_bytes));
    blobs.push((action_digest.clone(), action_bytes));
    upload_blobs(backend, blobs).await?;
    let sources: Vec<&Input> = inputs.values().collect();
    upload(backend, &sources).await?;

    if !no_cache {
        if let Some(outputs) = cached(backend, &action_digest, action).await? {
            return Ok(outputs);
        }
    }
    let request = executor::Action {
        digest: action_digest.clone(),
        command,
        input_root: tree.root,
        timeout: None,
        platform: Vec::new(),
    };
    tracing::info!(label = %owner.label, "{}", progress(action, owner));
    let result = backend
        .executor
        .execute(&request)
        .await
        .map_err(|error| SkyError::transient(format!("{}: {error}", progress(action, owner))))?;
    if result.exit_code != 0 {
        return Err(failed(backend, action, owner, &result).await);
    }
    let outputs = outputs(action, &result)?;
    if !no_cache {
        backend
            .action_cache
            .put(&action_digest, &result)
            .await
            .map_err(|error| SkyError::transient(format!("action cache: {error}")))?;
    }
    Ok(outputs)
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
/// other actions are spliced in from their `Tree`s.
async fn input_tree(backend: &Backend, inputs: &BTreeMap<String, Input>) -> Result<crate::inputs::MerkleTree, SkyError> {
    let mut tree = InputTree::default();
    let mut blobs = Vec::new();
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
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    for (digest, bytes) in blobs {
        if missing.contains(&digest) {
            backend.cas.put_bytes(&bytes).await.map_err(cas_error)?;
        }
    }
    Ok(())
}

fn command(action: &Action, spawn: &Spawn) -> reapi::Command {
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
    let environment_variables = spawn
        .env
        .iter()
        .map(|(name, value)| reapi::command::EnvironmentVariable {
            name: name.clone(),
            value: value.clone(),
            ..Default::default()
        })
        .collect();
    reapi::Command {
        arguments: spawn.arguments.clone(),
        environment_variables,
        output_files: files,
        output_directories: directories,
        ..Default::default()
    }
}

/// The outputs of a cached result, if there is one and its outputs are
/// still in the CAS.
async fn cached(backend: &Backend, digest: &cas::Digest, action: &Action) -> Result<Option<ActionOutputs>, SkyError> {
    let result = backend
        .action_cache
        .get(digest)
        .await
        .map_err(|error| SkyError::transient(format!("action cache: {error}")))?;
    let Some(result) = result.filter(|result| result.exit_code == 0) else {
        return Ok(None);
    };
    let Ok(outputs) = outputs(action, &result) else {
        return Ok(None);
    };
    let digests: Vec<cas::Digest> = outputs.files.iter().map(|output| output.digest.to_cas()).collect();
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    Ok(missing.is_empty().then_some(outputs))
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

async fn output_text(backend: &Backend, raw: &[u8], digest: Option<&reapi::Digest>) -> String {
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
