//! Running actions, as keys of the build graph.

use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::source::SourceDigestKey;
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTargetKey};
use bazel_fs::RootedPath;
use futures::StreamExt;
use bazel_loading::RepoRootKey;
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::Instrument;

/// An output an action generated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct OutputFile {
    /// The path relative to the execution root.
    pub path: PathId,
    /// The file's digest, or for a directory, the digest of its REAPI
    /// `Tree`.
    pub digest: BlobDigest,
    pub executable: bool,
    pub is_directory: bool,
}

/// What running an action generated.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ActionOutputs {
    pub files: Vec<OutputFile>,
}

encoded_value!(ActionOutputs);

impl ActionOutputs {
    pub fn get(&self, path: PathId) -> Option<&OutputFile> {
        self.files.iter().find(|output| output.path == path)
    }
}

/// Runs one of a configured target's actions.
///
/// The value is only the outputs' digests: the outputs themselves are in
/// the backend's CAS, and are written to the output tree when a build asks
/// for them. Workspaces whose actions have the same inputs share results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ActionKey {
    pub owner: ConfiguredTargetKey,
    pub index: u32,
}

impl Key for ActionKey {
    type Value = ActionOutputs;
    const KIND: &'static str = "ACTION_EXECUTION";

    async fn compute(&self, ctx: &Ctx) -> Result<ActionOutputs, SkyError> {
        let target = ctx.compute(&self.owner).await?;
        let Some(stored) = target.actions.get(self.index as usize) else {
            return Err(SkyError::new(format!("{} has no action {}", self.owner.label, self.index)));
        };
        let action = &stored.load()?;
        let backend = ctx.global::<Backend>()?;
        let span = tracing::info_span!("action", mnemonic = action.mnemonic.as_str(), label = %self.owner.label);
        run(ctx, &backend, self.owner, action).instrument(span).await
    }
}

async fn run(ctx: &Ctx, backend: &Arc<Backend>, owner: ConfiguredTargetKey, action: &Action) -> Result<ActionOutputs, SkyError> {
    match &action.kind {
        ActionKind::Write { content, executable } => write(backend, action, content.as_bytes(), *executable).await,
        ActionKind::Template {
            template,
            substitutions,
            executable,
        } => expand_template(ctx, backend, action, *template, substitutions, *executable).await,
        ActionKind::Symlink { target, executable } => copy(ctx, backend, action, *target, *executable).await,
        ActionKind::Spawn(spawn) => crate::spawn::run(ctx, backend, owner, action, spawn).await,
    }
}

/// An input an action reads: its digest, and where it is on disk if it's a
/// source file, to upload it from.
#[derive(Clone, Debug)]
pub struct Input {
    pub digest: BlobDigest,
    pub executable: bool,
    pub is_directory: bool,
    pub source: Option<PathBuf>,
}

/// The digest of an input: a source file's, or the output of the action
/// that generates it.
pub async fn input(ctx: &Ctx, artifact: Artifact) -> Result<Input, SkyError> {
    let Some(config) = artifact.config else {
        return source_input(ctx, artifact).await;
    };
    let owner = ConfiguredTargetKey {
        label: artifact.owner,
        config,
    };
    let target = ctx.compute(&owner).await?;
    let Some((index, _)) = target.generating_action(artifact.exec_path) else {
        return Err(SkyError::new(format!("{}: no action generates '{}'", artifact.owner, artifact.exec_path)));
    };
    let outputs = ctx.compute(&ActionKey { owner, index }).await?;
    let Some(output) = outputs.get(artifact.exec_path) else {
        return Err(SkyError::new(format!("{}: '{}' wasn't generated", artifact.owner, artifact.exec_path)));
    };
    Ok(Input {
        digest: output.digest,
        executable: output.executable,
        is_directory: output.is_directory,
        source: None,
    })
}

async fn source_input(ctx: &Ctx, artifact: Artifact) -> Result<Input, SkyError> {
    let label = artifact.owner;
    let root = ctx.compute(&RepoRootKey(label.repo())).await?;
    let path = label.package.path.join_path(bazel_analysis::name_path(label));
    let rooted = RootedPath::new(root.0, path);
    let digest = ctx.compute(&SourceDigestKey(rooted)).await?;
    let Some(blob) = digest.digest else {
        return Err(SkyError::new(format!("missing input file '{label}'")));
    };
    Ok(Input {
        digest: blob,
        executable: digest.executable,
        is_directory: false,
        source: Some(rooted.resolve(ctx)?),
    })
}

/// Make sure an input is in the CAS, uploading it if it's a source file.
pub async fn upload(backend: &Backend, inputs: &[&Input]) -> Result<(), SkyError> {
    let by_digest: HashMap<cas::Digest, &Input> = inputs.iter().map(|input| (input.digest.to_cas(), *input)).collect();
    let digests: Vec<cas::Digest> = by_digest.keys().cloned().collect();
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    let mut uploads = Vec::with_capacity(missing.len());
    for digest in missing {
        let Some(input) = by_digest.get(&digest) else {
            continue;
        };
        let Some(source) = &input.source else {
            return Err(SkyError::transient(format!("an action's output {digest} is no longer in the CAS")));
        };
        uploads.push(backend.cas.put_file(source));
    }
    let results: Vec<_> = futures::stream::iter(uploads).buffer_unordered(UPLOADS).collect().await;
    for result in results {
        result.map_err(cas_error)?;
    }
    Ok(())
}

/// How many blobs an action uploads at once.
pub const UPLOADS: usize = 16;

pub fn cas_error(error: cas::CasError) -> SkyError {
    SkyError::transient(format!("CAS: {error}"))
}

fn single_output(action: &Action) -> Result<Artifact, SkyError> {
    match action.outputs.as_slice() {
        [output] => Ok(*output),
        _ => Err(SkyError::new(format!("a {} action must have one output", action.mnemonic))),
    }
}

async fn write(backend: &Backend, action: &Action, content: &[u8], executable: bool) -> Result<ActionOutputs, SkyError> {
    let output = single_output(action)?;
    let digest = backend.cas.put_bytes(content).await.map_err(cas_error)?;
    Ok(ActionOutputs {
        files: vec![OutputFile {
            path: output.exec_path,
            digest: BlobDigest::from_cas(&digest),
            executable,
            is_directory: false,
        }],
    })
}

async fn expand_template(
    ctx: &Ctx,
    backend: &Arc<Backend>,
    action: &Action,
    template: Artifact,
    substitutions: &[(String, String)],
    executable: bool,
) -> Result<ActionOutputs, SkyError> {
    let template = input(ctx, template).await?;
    upload(backend, &[&template]).await?;
    let bytes = backend.cas.read_bytes(&template.digest.to_cas()).await.map_err(cas_error)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    for (from, to) in substitutions {
        text = text.replace(from.as_str(), to);
    }
    write(backend, action, text.as_bytes(), executable).await
}

async fn copy(ctx: &Ctx, backend: &Arc<Backend>, action: &Action, target: Artifact, executable: bool) -> Result<ActionOutputs, SkyError> {
    let output = single_output(action)?;
    let target = input(ctx, target).await?;
    upload(backend, &[&target]).await?;
    Ok(ActionOutputs {
        files: vec![OutputFile {
            path: output.exec_path,
            digest: target.digest,
            executable: executable || target.executable,
            is_directory: target.is_directory,
        }],
    })
}
