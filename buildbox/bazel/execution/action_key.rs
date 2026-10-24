//! Running actions, as keys of the build graph.

use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::source::{SourceDigestKey, SourceTree, SourceTreeKey};
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTargetKey};
use bazel_fs::{FileState, FileStateKey, RootedPath};
use futures::StreamExt;
use bazel_loading::RepoRootKey;
use bazel_progress::Progress;
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
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

impl ActionKey {
    /// A number identifying the action in progress reports.
    pub fn progress_id(&self) -> u64 {
        let mut hasher = std::hash::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
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
        run(ctx, &backend, *self, action).instrument(span).await
    }
}

async fn run(ctx: &Ctx, backend: &Arc<Backend>, key: ActionKey, action: &Action) -> Result<ActionOutputs, SkyError> {
    let ActionKind::Spawn(spawn) = &action.kind else {
        return run_internal(ctx, backend, key, action).await;
    };
    crate::spawn::run(ctx, backend, key, action, spawn).await
}

/// Run an action that Bazel would run in its own process, without
/// spawning one.
async fn run_internal(ctx: &Ctx, backend: &Arc<Backend>, key: ActionKey, action: &Action) -> Result<ActionOutputs, SkyError> {
    let read = match &action.kind {
        ActionKind::Template { template, .. } => Some(generated_input(ctx, *template).await?),
        ActionKind::Symlink { target, .. } => Some(generated_input(ctx, *target).await?),
        _ => None,
    };
    let inputs = read.iter().filter_map(|(_, generated_by)| generated_by.as_ref().map(ActionKey::progress_id)).collect();
    let shown = progress(ctx).map(|progress| progress.action_started(key.progress_id(), inputs, description(action, key.owner)));
    let outputs = match (&action.kind, read) {
        (ActionKind::Write { content, executable }, _) => write(backend, action, content.as_bytes(), *executable).await,
        (
            ActionKind::Template {
                substitutions,
                executable,
                ..
            },
            Some((template, _)),
        ) => expand_template(backend, action, template, substitutions, *executable).await,
        (ActionKind::Symlink { executable, .. }, Some((target, _))) => copy(backend, action, target, *executable).await,
        _ => Err(SkyError::new("an internal action's input wasn't read")),
    }?;
    if let Some(shown) = shown {
        shown.finish("internal");
    }
    Ok(outputs)
}

/// The graph's progress service, if it has one.
pub fn progress(ctx: &Ctx) -> Option<Arc<Progress>> {
    ctx.global::<Progress>().ok()
}

/// What progress output calls an action: its progress message, or its
/// mnemonic and owner.
pub fn description(action: &Action, owner: ConfiguredTargetKey) -> String {
    match action.progress_message.is_empty() {
        true => format!("{} {}", action.mnemonic, owner.label),
        false => action.progress_message.clone(),
    }
}

/// An input an action reads: its digest, and where it is on disk if it's a
/// source file or directory, to upload it from.
#[derive(Clone, Debug)]
pub struct Input {
    pub digest: BlobDigest,
    pub executable: bool,
    pub is_directory: bool,
    pub source: Option<PathBuf>,
    /// A source directory's contents, which go in the input tree file by
    /// file.
    pub tree: Option<Arc<SourceTree>>,
}

impl Input {
    /// The source files to upload for this input, if it's in the CAS as
    /// `digest`: itself, or each file of its source tree.
    fn sources(&self) -> Vec<(cas::Digest, Option<PathBuf>)> {
        let Some(tree) = &self.tree else {
            return vec![(self.digest.to_cas(), self.source.clone())];
        };
        let Some(dir) = &self.source else {
            return Vec::new();
        };
        tree.files.iter().map(|(path, digest, _)| (digest.to_cas(), Some(dir.join(path)))).collect()
    }
}

/// The digest of an input: a source file's, or the output of the action
/// that generates it.
pub async fn input(ctx: &Ctx, artifact: Artifact) -> Result<Input, SkyError> {
    let (input, _) = generated_input(ctx, artifact).await?;
    Ok(input)
}

/// [`input`], with the action that generated it, if it isn't a source
/// file.
pub async fn generated_input(ctx: &Ctx, artifact: Artifact) -> Result<(Input, Option<ActionKey>), SkyError> {
    let Some(config) = artifact.config else {
        return Ok((source_input(ctx, artifact).await?, None));
    };
    let owner = ConfiguredTargetKey {
        label: artifact.owner,
        config,
    };
    let target = ctx.compute(&owner).await?;
    let Some((index, _)) = target.generating_action(artifact.exec_path) else {
        return Err(SkyError::new(format!("{}: no action generates '{}'", artifact.owner, artifact.exec_path)));
    };
    let key = ActionKey { owner, index };
    let outputs = ctx.compute(&key).await?;
    let Some(output) = outputs.get(artifact.exec_path) else {
        return Err(SkyError::new(format!("{}: '{}' wasn't generated", artifact.owner, artifact.exec_path)));
    };
    let input = Input {
        digest: output.digest,
        executable: output.executable,
        is_directory: output.is_directory,
        source: None,
        tree: None,
    };
    Ok((input, Some(key)))
}

async fn source_input(ctx: &Ctx, artifact: Artifact) -> Result<Input, SkyError> {
    let label = artifact.owner;
    let root = ctx.compute(&RepoRootKey(label.repo())).await?;
    let path = label.package.path.join_path(bazel_analysis::name_path(label));
    let rooted = RootedPath::new(root.0, path);
    let digest = ctx.compute(&SourceDigestKey(rooted)).await?;
    let Some(blob) = digest.digest else {
        return source_directory_input(ctx, rooted, label).await;
    };
    Ok(Input {
        digest: blob,
        executable: digest.executable,
        is_directory: false,
        source: Some(rooted.resolve(ctx)?),
        tree: None,
    })
}

/// A source input that isn't a file: a directory, read as a whole, or
/// missing.
async fn source_directory_input(ctx: &Ctx, rooted: RootedPath, label: bazel_label::Label) -> Result<Input, SkyError> {
    let state = ctx.compute(&FileStateKey(rooted)).await?;
    if *state != FileState::Directory {
        return Err(SkyError::new(format!("{MISSING_INPUT}'{label}'")));
    }
    let tree = ctx.compute(&SourceTreeKey(rooted)).await?;
    Ok(Input {
        digest: tree.digest(),
        executable: false,
        is_directory: true,
        source: Some(rooted.resolve(ctx)?),
        tree: Some(tree),
    })
}

/// How the error for a source file that doesn't exist starts.
pub const MISSING_INPUT: &str = "missing input file ";

/// Make sure an input is in the CAS, uploading it if it's a source file.
pub async fn upload(backend: &Backend, inputs: &[&Input]) -> Result<(), SkyError> {
    let by_digest: HashMap<cas::Digest, Option<PathBuf>> = inputs.iter().flat_map(|input| input.sources()).collect();
    let digests: Vec<cas::Digest> = by_digest.keys().cloned().collect();
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    let mut uploads = Vec::with_capacity(missing.len());
    for digest in missing {
        let Some(source) = by_digest.get(&digest) else {
            continue;
        };
        let Some(source) = source else {
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
    backend: &Arc<Backend>,
    action: &Action,
    template: Input,
    substitutions: &[(String, String)],
    executable: bool,
) -> Result<ActionOutputs, SkyError> {
    upload(backend, &[&template]).await?;
    let bytes = backend.cas.read_bytes(&template.digest.to_cas()).await.map_err(cas_error)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    for (from, to) in substitutions {
        text = text.replace(from.as_str(), to);
    }
    write(backend, action, text.as_bytes(), executable).await
}

async fn copy(backend: &Arc<Backend>, action: &Action, target: Input, executable: bool) -> Result<ActionOutputs, SkyError> {
    let output = single_output(action)?;
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
