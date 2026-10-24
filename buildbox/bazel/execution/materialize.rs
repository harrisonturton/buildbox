//! Writing outputs from the CAS into an output tree.

use crate::action_key::{OutputFile, cas_error};
use crate::backend::Backend;
use buffa::Message;
use remote_execution_rust_connect::build::bazel::remote::execution::v2 as reapi;
use skyframe::SkyError;
use std::path::{Path, PathBuf};

/// Write outputs beneath `exec_root`, replacing what's there.
pub async fn materialize(backend: &Backend, exec_root: &Path, outputs: &[OutputFile]) -> Result<(), SkyError> {
    for output in outputs {
        let dest = exec_root.join(output.path.to_string());
        remove(&dest).await?;
        match output.is_directory {
            false => write_file(backend, &output.digest.to_cas(), &dest, output.executable).await?,
            true => write_tree(backend, &output.digest.to_cas(), &dest).await?,
        }
    }
    Ok(())
}

async fn remove(path: &Path) -> Result<(), SkyError> {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("stat", path, error)),
    };
    let result = match metadata.is_dir() {
        true => tokio::fs::remove_dir_all(path).await,
        false => tokio::fs::remove_file(path).await,
    };
    result.map_err(|error| io_error("remove", path, error))
}

async fn write_file(backend: &Backend, digest: &cas::Digest, dest: &Path, executable: bool) -> Result<(), SkyError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|error| io_error("create", parent, error))?;
    }
    backend.cas.materialize(digest, dest, executable).await.map_err(cas_error)
}

/// Write a directory output from its `Tree`.
async fn write_tree(backend: &Backend, digest: &cas::Digest, dest: &Path) -> Result<(), SkyError> {
    let bytes = backend.cas.read_bytes(digest).await.map_err(cas_error)?;
    let tree = reapi::Tree::decode_from_slice(&bytes).map_err(|error| SkyError::new(format!("invalid Tree {digest}: {error}")))?;
    let mut children = std::collections::HashMap::new();
    for child in &tree.children {
        children.insert(cas::Digest::of_bytes(&child.encode_to_vec()), child.clone());
    }
    let root = tree.root.as_option().cloned().unwrap_or_default();
    let mut pending: Vec<(PathBuf, reapi::Directory)> = vec![(dest.to_path_buf(), root)];
    while let Some((path, directory)) = pending.pop() {
        tokio::fs::create_dir_all(&path).await.map_err(|error| io_error("create", &path, error))?;
        for file in &directory.files {
            let Some(digest) = file.digest.as_option().and_then(|digest| cas::Digest::from_proto(digest).ok()) else {
                continue;
            };
            write_file(backend, &digest, &path.join(&file.name), file.is_executable).await?;
        }
        for child in &directory.directories {
            let child_digest = child.digest.as_option().and_then(|digest| cas::Digest::from_proto(digest).ok());
            let Some(child_directory) = child_digest.and_then(|digest| children.get(&digest)) else {
                return Err(SkyError::new(format!("Tree {digest} is missing directory '{}'", child.name)));
            };
            pending.push((path.join(&child.name), child_directory.clone()));
        }
    }
    Ok(())
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> SkyError {
    SkyError::transient(format!("failed to {action} {}: {error}", path.display()))
}
