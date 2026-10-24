//! Preparing for, and collecting, an action's outputs.

use crate::paths;
use buffa::Message;
use cas::{Cas, Digest};
use executor::ExecuteError;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    Command, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile, SymlinkNode, Tree,
};
use std::fs::Metadata;
use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tracing::field::Empty;
use tracing::Span;

/// The outputs an action produced, stored in the CAS.
#[derive(Debug, Default)]
pub struct Outputs {
    pub files: Vec<OutputFile>,
    pub directories: Vec<OutputDirectory>,
}

/// Create the parent directories of every output, which the protocol
/// requires to exist before the command runs. The outputs themselves aren't
/// created.
pub async fn create_output_parents(command: &Command, working_dir: &Path) -> Result<(), ExecuteError> {
    let outputs = command.output_files.iter().chain(&command.output_directories);
    for output in outputs {
        let path = parse_output_path(output)?;
        let Some(parent) = path.parent() else {
            continue;
        };

        let parent = working_dir.join(parent);
        tokio::fs::create_dir_all(&parent).await.map_err(|error| {
            ExecuteError::Internal(format!("failed to create {}: {error}", parent.display()))
        })?;
    }
    Ok(())
}

/// Store the outputs the command produced in the CAS. Outputs that weren't
/// produced are left out.
#[tracing::instrument(skip_all, fields(files = Empty, directories = Empty))]
pub async fn collect_outputs(cas: &dyn Cas, command: &Command, working_dir: &Path) -> Result<Outputs, ExecuteError> {
    let mut outputs = Outputs::default();

    for output in &command.output_files {
        if let Some(file) = collect_file(cas, output, working_dir).await? {
            outputs.files.push(file);
        }
    }

    for output in &command.output_directories {
        if let Some(directory) = collect_directory(cas, output, working_dir).await? {
            outputs.directories.push(directory);
        }
    }

    let span = Span::current();
    span.record("files", outputs.files.len());
    span.record("directories", outputs.directories.len());
    Ok(outputs)
}

async fn collect_file(cas: &dyn Cas, output: &str, working_dir: &Path) -> Result<Option<OutputFile>, ExecuteError> {
    let path = working_dir.join(parse_output_path(output)?);
    let Some(metadata) = metadata_if_exists(&path).await? else {
        return Ok(None);
    };
    if metadata.is_dir() {
        return Err(ExecuteError::InvalidOutput(format!("output file {output:?} is a directory")));
    }

    let digest = cas.put_file(&path).await?;
    Ok(Some(OutputFile {
        path: output.to_string(),
        digest: digest.to_proto().into(),
        is_executable: is_executable(&metadata),
        ..Default::default()
    }))
}

async fn collect_directory(
    cas: &dyn Cas,
    output: &str,
    working_dir: &Path,
) -> Result<Option<OutputDirectory>, ExecuteError> {
    let path = working_dir.join(parse_output_path(output)?);
    let Some(metadata) = metadata_if_exists(&path).await? else {
        return Ok(None);
    };
    if !metadata.is_dir() {
        return Err(ExecuteError::InvalidOutput(format!("output directory {output:?} isn't a directory")));
    }

    let mut children = vec![];
    let root = build_directory(cas, path, &mut children).await?;
    let tree = Tree {
        root: root.into(),
        children,
        ..Default::default()
    };
    let tree_digest = cas.put_bytes(&tree.encode_to_vec()).await?;

    Ok(Some(OutputDirectory {
        path: output.to_string(),
        tree_digest: tree_digest.to_proto().into(),
        ..Default::default()
    }))
}

/// Build the `Directory` proto for `path`, storing its files in the CAS and
/// adding every subdirectory's `Directory` to `children`.
fn build_directory<'a>(
    cas: &'a dyn Cas,
    path: PathBuf,
    children: &'a mut Vec<Directory>,
) -> Pin<Box<dyn Future<Output = Result<Directory, ExecuteError>> + Send + 'a>> {
    Box::pin(async move {
        let mut directory = Directory::default();

        // Directory entries must be sorted by name.
        for (name, entry_path) in sorted_entries(&path).await? {
            let metadata = tokio::fs::symlink_metadata(&entry_path)
                .await
                .map_err(|error| io_error("failed to stat", &entry_path, error))?;

            if metadata.is_symlink() {
                let target = tokio::fs::read_link(&entry_path)
                    .await
                    .map_err(|error| io_error("failed to read symlink", &entry_path, error))?;
                directory.symlinks.push(SymlinkNode {
                    name,
                    target: target.to_string_lossy().into_owned(),
                    ..Default::default()
                });
            } else if metadata.is_dir() {
                let child = build_directory(cas, entry_path, children).await?;
                let digest = Digest::of_bytes(&child.encode_to_vec());
                children.push(child);
                directory.directories.push(DirectoryNode {
                    name,
                    digest: digest.to_proto().into(),
                    ..Default::default()
                });
            } else {
                let digest = cas.put_file(&entry_path).await?;
                directory.files.push(FileNode {
                    name,
                    digest: digest.to_proto().into(),
                    is_executable: is_executable(&metadata),
                    ..Default::default()
                });
            }
        }
        Ok(directory)
    })
}

async fn sorted_entries(path: &Path) -> Result<Vec<(String, PathBuf)>, ExecuteError> {
    let mut reader = tokio::fs::read_dir(path)
        .await
        .map_err(|error| io_error("failed to list", path, error))?;

    let mut entries = vec![];
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|error| io_error("failed to list", path, error))?
    {
        let Ok(name) = entry.file_name().into_string() else {
            let message = format!("output {} has a name that isn't UTF-8", entry.path().display());
            return Err(ExecuteError::InvalidOutput(message));
        };
        entries.push((name, entry.path()));
    }
    entries.sort();
    Ok(entries)
}

fn parse_output_path(output: &str) -> Result<PathBuf, ExecuteError> {
    let path = paths::parse_relative_path(output).map_err(ExecuteError::InvalidAction)?;
    if path.as_os_str().is_empty() {
        return Err(ExecuteError::InvalidAction(format!("invalid output path {output:?}")));
    }
    Ok(path)
}

/// Metadata for `path`, following symlinks, or `None` if it doesn't exist.
async fn metadata_if_exists(path: &Path) -> Result<Option<Metadata>, ExecuteError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("failed to stat", path, error)),
    }
}

fn is_executable(metadata: &Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ExecuteError {
    ExecuteError::Internal(format!("{action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;

    fn command_with_outputs(files: &[&str], directories: &[&str]) -> Command {
        Command {
            output_files: files.iter().map(|path| path.to_string()).collect(),
            output_directories: directories.iter().map(|path| path.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn create_output_parents_creates_parents_only() {
        let dir = tempfile::tempdir().unwrap();
        let command = command_with_outputs(&["a/b/file"], &["c/d"]);

        create_output_parents(&command, dir.path()).await.unwrap();

        assert!(dir.path().join("a/b").is_dir());
        assert!(!dir.path().join("a/b/file").exists());
        assert!(dir.path().join("c").is_dir());
        assert!(!dir.path().join("c/d").exists());
    }

    #[tokio::test]
    async fn create_output_parents_rejects_escaping_paths() {
        let dir = tempfile::tempdir().unwrap();
        let command = command_with_outputs(&["../outside"], &[]);

        let err = create_output_parents(&command, dir.path()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }

    #[tokio::test]
    async fn collects_files_and_skips_missing_ones() {
        let cas = MemCas::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.txt"), b"output").unwrap();
        let command = command_with_outputs(&["out.txt", "missing.txt"], &[]);

        let outputs = collect_outputs(&cas, &command, dir.path()).await.unwrap();

        assert_eq!(outputs.files.len(), 1);
        let file = &outputs.files[0];
        assert_eq!(file.path, "out.txt");
        assert_eq!(file.digest, Digest::of_bytes(b"output").to_proto().into());
        assert!(!file.is_executable);
        assert_eq!(cas.read_bytes(&Digest::of_bytes(b"output")).await.unwrap(), b"output");
    }

    #[tokio::test]
    async fn reports_executable_files() {
        let cas = MemCas::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        std::fs::write(&path, b"#!/bin/sh").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let command = command_with_outputs(&["tool"], &[]);

        let outputs = collect_outputs(&cas, &command, dir.path()).await.unwrap();

        assert!(outputs.files[0].is_executable);
    }

    #[tokio::test]
    async fn rejects_directory_where_file_expected() {
        let cas = MemCas::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        let command = command_with_outputs(&["out"], &[]);

        let err = collect_outputs(&cas, &command, dir.path()).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidOutput(_)), "{err}");
    }

    #[tokio::test]
    async fn collects_directories_as_trees() {
        let cas = MemCas::new();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(out.join("nested")).unwrap();
        std::fs::write(out.join("b.txt"), b"b").unwrap();
        std::fs::write(out.join("a.txt"), b"a").unwrap();
        std::fs::write(out.join("nested/c.txt"), b"c").unwrap();
        std::os::unix::fs::symlink("a.txt", out.join("link")).unwrap();
        let command = command_with_outputs(&[], &["out"]);

        let outputs = collect_outputs(&cas, &command, dir.path()).await.unwrap();

        let tree_digest = Digest::from_required_proto(outputs.directories[0].tree_digest.as_option()).unwrap();
        let tree = Tree::decode_from_slice(&cas.read_bytes(&tree_digest).await.unwrap()).unwrap();
        let root = tree.root.unwrap();
        let file_names: Vec<&str> = root.files.iter().map(|file| file.name.as_str()).collect();
        assert_eq!(file_names, ["a.txt", "b.txt"]);
        assert_eq!(root.directories[0].name, "nested");
        assert_eq!(root.symlinks[0].name, "link");
        assert_eq!(root.symlinks[0].target, "a.txt");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].files[0].name, "c.txt");
        assert_eq!(cas.read_bytes(&Digest::of_bytes(b"c")).await.unwrap(), b"c");
    }
}
