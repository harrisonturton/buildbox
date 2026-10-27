//! Running actions, as keys of the build graph.

use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::source::{SourceDigestKey, SourceTree, SourceTreeKey};
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTargetKey};
use bazel_fs::{FileState, FileStateKey, RootedPath, Workspace};
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
        (ActionKind::Write { content, executable, .. }, _) => write(backend, action, content.as_bytes(), *executable).await,
        (
            ActionKind::Template {
                substitutions,
                executable,
                ..
            },
            Some((template, _)),
        ) => expand_template(backend, action, template, substitutions, *executable).await,
        (ActionKind::Symlink { executable, .. }, Some((target, _))) => copy(backend, action, target, *executable).await,
        (ActionKind::RepoMappingManifest { files }, _) => {
            let content = bazel_analysis::repo_mapping_manifest(ctx, files.iter().copied(), None).await?;
            write(backend, action, content.as_bytes(), false).await
        }
        (ActionKind::SymlinkTree { .. } | ActionKind::RunfilesTree, _) => Err(SkyError::new("bb lays out runfiles trees as it runs their executables")),
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

/// What progress output calls an action, as Bazel does: see
/// [`Action::description`].
pub fn description(action: &Action, owner: ConfiguredTargetKey) -> String {
    action.description(&owner)
}

/// An input an action reads: its digest, and where it is on disk if it's a
/// source file or directory, to upload it from.
#[derive(Clone, Debug)]
pub struct Input {
    pub digest: BlobDigest,
    pub executable: bool,
    pub is_directory: bool,
    pub source: Option<SourceLocation>,
    /// A source directory's contents, which go in the input tree file by
    /// file.
    pub tree: Option<Arc<SourceTree>>,
}

impl Input {
    /// The source files to upload for this input, if it's in the CAS as
    /// `digest`: itself, or each file of its source tree.
    fn sources(&self) -> Vec<(BlobDigest, Option<SourceFile<'_>>)> {
        let Some(tree) = &self.tree else {
            let file = self.source.as_ref().map(|location| SourceFile { location, within: None });
            return vec![(self.digest, file)];
        };
        let Some(location) = &self.source else {
            return Vec::new();
        };
        let files = tree.files.iter().map(|(path, digest, _)| {
            let file = SourceFile { location, within: Some(path.as_str()) };
            (*digest, Some(file))
        });
        files.collect()
    }
}

/// Where a source file or directory is. Actions hold one for each source
/// input while they're prepared, so it's resolved to a path on disk only
/// when it's uploaded.
#[derive(Clone)]
pub struct SourceLocation {
    workspace: Arc<Workspace>,
    path: RootedPath,
}

impl SourceLocation {
    fn new(ctx: &Ctx, path: RootedPath) -> Result<SourceLocation, SkyError> {
        let workspace = ctx.global::<Workspace>()?;
        Ok(SourceLocation { workspace, path })
    }

    /// The path on disk.
    fn resolve(&self) -> PathBuf {
        self.path.resolve_in(&self.workspace.root)
    }
}

impl std::fmt::Debug for SourceLocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.path)
    }
}

/// A source file to upload: a source input, or a file in a source
/// directory.
#[derive(Clone, Copy)]
struct SourceFile<'a> {
    location: &'a SourceLocation,
    /// The file's path in the directory, if it's in one.
    within: Option<&'a str>,
}

impl SourceFile<'_> {
    fn resolve(&self) -> PathBuf {
        let path = self.location.resolve();
        match self.within {
            Some(within) => path.join(within),
            None => path,
        }
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
        source: Some(SourceLocation::new(ctx, rooted)?),
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
        source: Some(SourceLocation::new(ctx, rooted)?),
        tree: Some(tree),
    })
}

/// How the error for a source file that doesn't exist starts.
pub const MISSING_INPUT: &str = "missing input file ";

/// Make sure an input is in the CAS, uploading it if it's a source file.
pub async fn upload(backend: &Backend, inputs: &[&Input]) -> Result<(), SkyError> {
    let by_digest: HashMap<BlobDigest, Option<SourceFile<'_>>> = inputs.iter().flat_map(|input| input.sources()).collect();
    let missing = find_missing(backend, by_digest.keys()).await?;
    let mut sources = Vec::with_capacity(missing.len());
    for digest in missing {
        let Some(source) = by_digest.get(&digest) else {
            continue;
        };
        let Some(source) = source else {
            return Err(SkyError::transient(format!("an action's output {} is no longer in the CAS", digest.to_cas())));
        };
        sources.push((digest, *source));
    }
    upload_sources(backend, sources).await
}

/// Upload source files, [`UPLOADS`] at a time, resolving each one's path
/// only as its upload starts.
async fn upload_sources(backend: &Backend, sources: Vec<(BlobDigest, SourceFile<'_>)>) -> Result<(), SkyError> {
    let mut pending = sources.into_iter();
    let mut running = futures::stream::FuturesUnordered::new();
    let mut failed = None;
    loop {
        while running.len() < UPLOADS {
            let Some((digest, source)) = pending.next() else {
                break;
            };
            running.push(backend.upload_source(digest, source.resolve()));
        }
        let Some(result) = running.next().await else {
            break;
        };
        if let Err(error) = result {
            failed.get_or_insert(error);
        }
    }
    match failed {
        Some(error) => Err(cas_error(error)),
        None => Ok(()),
    }
}

/// Which of `digests` the CAS lacks.
async fn find_missing(backend: &Backend, digests: impl Iterator<Item = &BlobDigest>) -> Result<Vec<BlobDigest>, SkyError> {
    let digests: Vec<cas::Digest> = digests.map(BlobDigest::to_cas).collect();
    let missing = backend.cas.find_missing(&digests).await.map_err(cas_error)?;
    Ok(missing.iter().map(BlobDigest::from_cas).collect())
}

/// How many blobs an action uploads at once.
pub const UPLOADS: usize = 16;

/// An input of fixed `contents`, which this puts in the CAS.
pub async fn blob_input(ctx: &Ctx, contents: &[u8]) -> Result<Input, SkyError> {
    let backend = ctx.global::<Backend>()?;
    let digest = backend.cas.put_bytes(contents).await.map_err(cas_error)?;
    Ok(Input {
        digest: BlobDigest::from_cas(&digest),
        executable: false,
        is_directory: false,
        source: None,
        tree: None,
    })
}

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
