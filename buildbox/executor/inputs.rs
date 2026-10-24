//! Laying out an action's input tree in its sandbox.

use crate::paths;
use buffa::Message;
use cas::{Cas, CasError, Digest, Materialization};
use executor::ExecuteError;
use fsutil::run_blocking;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::Directory;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::field::Empty;
use tracing::Span;

/// The entries of an input tree, with paths relative to the input root.
/// Directories are listed before anything inside them.
#[derive(Debug, Default)]
struct InputTree {
    dirs: Vec<PathBuf>,
    files: Vec<InputFile>,
    symlinks: Vec<InputSymlink>,
}

#[derive(Debug)]
struct InputFile {
    path: PathBuf,
    digest: Digest,
    executable: bool,
}

#[derive(Debug)]
struct InputSymlink {
    path: PathBuf,
    target: String,
}

/// Create the input tree rooted at the `Directory` with digest `input_root`
/// inside `sandbox`.
///
/// Fails with [`ExecuteError::MissingInputs`], listing every missing blob, if
/// any part of the tree isn't in the CAS.
#[tracing::instrument(skip_all, fields(dirs = Empty, files = Empty, symlinks = Empty))]
pub async fn stage_inputs(cas: &dyn Cas, directories: &DirectoryCache, input_root: &Digest, sandbox: &Path) -> Result<(), ExecuteError> {
    let tree = resolve_tree(cas, directories, input_root).await?;
    let span = Span::current();
    span.record("dirs", tree.dirs.len());
    span.record("files", tree.files.len());
    span.record("symlinks", tree.symlinks.len());

    check_files_present(cas, &tree).await?;
    create_entries(cas, tree, sandbox).await
}

/// Fetch every `Directory` in the tree and flatten it into a list of entries.
/// The directories of each level are fetched together.
#[tracing::instrument(skip_all)]
async fn resolve_tree(cas: &dyn Cas, directories: &DirectoryCache, input_root: &Digest) -> Result<InputTree, ExecuteError> {
    let mut tree = InputTree::default();
    let mut level = vec![(PathBuf::new(), input_root.clone())];

    while !level.is_empty() {
        let fetched = futures::future::try_join_all(level.iter().map(|(_, digest)| directories.get(cas, digest))).await?;
        let mut next = Vec::new();
        for ((path, _), directory) in level.into_iter().zip(fetched) {
            add_entries(&mut tree, &mut next, &path, &directory)?;
            tree.dirs.push(path);
        }
        level = next;
    }
    Ok(tree)
}

/// The `Directory` messages input trees are made of, which actions share
/// many of, such as a toolchain's, kept so they're read from the CAS once.
#[derive(Default)]
pub struct DirectoryCache {
    directories: Mutex<CachedDirectories>,
}

#[derive(Default)]
struct CachedDirectories {
    by_digest: HashMap<Digest, Arc<Directory>>,
    bytes: u64,
}

/// How many bytes of encoded directories the cache holds before it's
/// emptied and starts again.
const MAX_CACHED_DIRECTORY_BYTES: u64 = 64 << 20;

impl DirectoryCache {
    async fn get(&self, cas: &dyn Cas, digest: &Digest) -> Result<Arc<Directory>, ExecuteError> {
        if let Some(directory) = self.lock().by_digest.get(digest) {
            return Ok(directory.clone());
        }
        let directory = Arc::new(read_directory(cas, digest).await?);
        let mut cached = self.lock();
        if cached.bytes + digest.size_bytes() > MAX_CACHED_DIRECTORY_BYTES {
            *cached = CachedDirectories::default();
        }
        cached.bytes += digest.size_bytes();
        cached.by_digest.insert(digest.clone(), directory.clone());
        Ok(directory)
    }

    fn lock(&self) -> MutexGuard<'_, CachedDirectories> {
        self.directories.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

async fn read_directory(cas: &dyn Cas, digest: &Digest) -> Result<Directory, ExecuteError> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(digest)) => return Err(ExecuteError::MissingInputs(vec![digest])),
        Err(error) => return Err(error.into()),
    };

    Directory::decode_from_slice(&data)
        .map_err(|error| ExecuteError::InvalidAction(format!("input directory {digest} isn't a Directory: {error}")))
}

/// Add the entries of `directory`, found at `path`, to `tree`. Subdirectories
/// are added to `pending`, to be fetched.
fn add_entries(
    tree: &mut InputTree,
    pending: &mut Vec<(PathBuf, Digest)>,
    path: &Path,
    directory: &Directory,
) -> Result<(), ExecuteError> {
    for file in &directory.files {
        paths::validate_name(&file.name).map_err(ExecuteError::InvalidAction)?;
        tree.files.push(InputFile {
            path: path.join(&file.name),
            digest: parse_digest(file.digest.as_option(), &file.name)?,
            executable: file.is_executable,
        });
    }

    for symlink in &directory.symlinks {
        paths::validate_name(&symlink.name).map_err(ExecuteError::InvalidAction)?;
        tree.symlinks.push(InputSymlink {
            path: path.join(&symlink.name),
            target: symlink.target.clone(),
        });
    }

    for child in &directory.directories {
        paths::validate_name(&child.name).map_err(ExecuteError::InvalidAction)?;
        let digest = parse_digest(child.digest.as_option(), &child.name)?;
        pending.push((path.join(&child.name), digest));
    }
    Ok(())
}

fn parse_digest(
    proto: Option<&remote_execution_rust_connect::build::bazel::remote::execution::v2::Digest>,
    name: &str,
) -> Result<Digest, ExecuteError> {
    Digest::from_required_proto(proto)
        .map_err(|error| ExecuteError::InvalidAction(format!("input {name:?} has an invalid digest: {error}")))
}

/// Check for every missing file up front, so the client can upload them all
/// at once.
#[tracing::instrument(skip_all)]
async fn check_files_present(cas: &dyn Cas, tree: &InputTree) -> Result<(), ExecuteError> {
    let digests: BTreeSet<Digest> = tree.files.iter().map(|file| file.digest.clone()).collect();
    let digests: Vec<Digest> = digests.into_iter().collect();

    let missing = cas.find_missing(&digests).await?;
    if !missing.is_empty() {
        return Err(ExecuteError::MissingInputs(missing));
    }
    Ok(())
}

/// Create the directories, then the files, then the symlinks, each with a
/// few blocking calls rather than one for each entry.
#[tracing::instrument(skip_all)]
async fn create_entries(cas: &dyn Cas, tree: InputTree, sandbox: &Path) -> Result<(), ExecuteError> {
    let dirs: Vec<PathBuf> = tree.dirs.iter().map(|dir| sandbox.join(dir)).collect();
    run_blocking(move || create_dirs(&dirs)).await?;

    let files = tree
        .files
        .into_iter()
        .map(|file| Materialization {
            dest: sandbox.join(&file.path),
            digest: file.digest,
            executable: file.executable,
        })
        .collect();
    cas.materialize_all(files).await?;

    let symlinks: Vec<(PathBuf, String)> = tree.symlinks.into_iter().map(|symlink| (sandbox.join(&symlink.path), symlink.target)).collect();
    run_blocking(move || create_symlinks(&symlinks)).await
}

/// Create directories, which are listed each after its parent. The first
/// is the sandbox itself, which already exists.
fn create_dirs(dirs: &[PathBuf]) -> Result<(), ExecuteError> {
    for path in dirs.iter().skip(1) {
        std::fs::create_dir(path).map_err(|error| internal_error("failed to create", path, error))?;
    }
    Ok(())
}

fn create_symlinks(symlinks: &[(PathBuf, String)]) -> Result<(), ExecuteError> {
    for (path, target) in symlinks {
        std::os::unix::fs::symlink(target, path).map_err(|error| internal_error("failed to create symlink", path, error))?;
    }
    Ok(())
}

fn internal_error(action: &str, path: &Path, error: std::io::Error) -> ExecuteError {
    ExecuteError::Internal(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::{DirectoryNode, FileNode, SymlinkNode};

    async fn put_directory(cas: &MemCas, directory: &Directory) -> Digest {
        cas.put_bytes(&directory.encode_to_vec()).await.unwrap()
    }

    fn file_node(name: &str, digest: &Digest, executable: bool) -> FileNode {
        FileNode {
            name: name.to_string(),
            digest: digest.to_proto().into(),
            is_executable: executable,
            ..Default::default()
        }
    }

    fn dir_node(name: &str, digest: &Digest) -> DirectoryNode {
        DirectoryNode {
            name: name.to_string(),
            digest: digest.to_proto().into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn stages_files_directories_and_symlinks() {
        let cas = MemCas::new();
        let script = cas.put_bytes(b"#!/bin/sh\n").await.unwrap();
        let data = cas.put_bytes(b"data").await.unwrap();
        let subdir = put_directory(
            &cas,
            &Directory {
                files: vec![file_node("data.txt", &data, false)],
                ..Default::default()
            },
        )
        .await;
        let root = put_directory(
            &cas,
            &Directory {
                files: vec![file_node("run.sh", &script, true)],
                directories: vec![dir_node("sub", &subdir)],
                symlinks: vec![SymlinkNode {
                    name: "link".to_string(),
                    target: "sub/data.txt".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .await;
        let sandbox = tempfile::tempdir().unwrap();

        stage_inputs(&cas, &DirectoryCache::default(), &root, sandbox.path()).await.unwrap();

        assert_eq!(std::fs::read(sandbox.path().join("run.sh")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(std::fs::read(sandbox.path().join("sub/data.txt")).unwrap(), b"data");
        assert_eq!(std::fs::read(sandbox.path().join("link")).unwrap(), b"data");
        let link_target = std::fs::read_link(sandbox.path().join("link")).unwrap();
        assert_eq!(link_target, PathBuf::from("sub/data.txt"));
    }

    #[tokio::test]
    async fn reports_all_missing_files() {
        let cas = MemCas::new();
        let missing_a = Digest::of_bytes(b"a");
        let missing_b = Digest::of_bytes(b"b");
        let root = put_directory(
            &cas,
            &Directory {
                files: vec![file_node("a", &missing_a, false), file_node("b", &missing_b, false)],
                ..Default::default()
            },
        )
        .await;
        let sandbox = tempfile::tempdir().unwrap();

        let err = stage_inputs(&cas, &DirectoryCache::default(), &root, sandbox.path()).await.unwrap_err();

        let ExecuteError::MissingInputs(mut missing) = err else {
            panic!("unexpected error: {err}");
        };
        missing.sort();
        let mut expected = vec![missing_a, missing_b];
        expected.sort();
        assert_eq!(missing, expected);
    }

    #[tokio::test]
    async fn reports_missing_directory() {
        let cas = MemCas::new();
        let missing = Digest::of_bytes(b"not a directory");
        let sandbox = tempfile::tempdir().unwrap();

        let err = stage_inputs(&cas, &DirectoryCache::default(), &missing, sandbox.path()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::MissingInputs(digests) if digests == vec![missing]));
    }

    #[tokio::test]
    async fn rejects_names_that_escape_the_sandbox() {
        let cas = MemCas::new();
        let data = cas.put_bytes(b"data").await.unwrap();
        let root = put_directory(
            &cas,
            &Directory {
                files: vec![file_node("../escape", &data, false)],
                ..Default::default()
            },
        )
        .await;
        let sandbox = tempfile::tempdir().unwrap();

        let err = stage_inputs(&cas, &DirectoryCache::default(), &root, sandbox.path()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }
}
