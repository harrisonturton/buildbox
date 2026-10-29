//! Expanding the directories on command lines into their files, once the
//! actions that make them have run.

use crate::action_key::cas_error;
use crate::backend::Backend;
use crate::digest::BlobDigest;
use bazel_analysis::command_line::{self, DirectoryArgs};
use buffa::Message;
use intern::PathId;
use remote_execution_rust_connect::build::bazel::remote::execution::v2 as reapi;
use skyframe::SkyError;
use std::collections::HashMap;

/// `arguments` with the arguments made from directories expanded, each
/// directory's `Tree` given by `trees`, as Bazel expands them: each file's
/// path relative to the execution root, in order of path.
pub async fn expand(backend: &Backend, arguments: &[String], directories: &[DirectoryArgs], trees: &HashMap<PathId, BlobDigest>) -> Result<Vec<String>, SkyError> {
    let mut files: HashMap<PathId, Vec<String>> = HashMap::new();
    for directory in directories.iter().flat_map(DirectoryArgs::directories) {
        if files.contains_key(&directory.exec_path) {
            continue;
        }
        let Some(tree) = trees.get(&directory.exec_path) else {
            return Err(SkyError::new(format!(
                "can't expand directory '{}' on the command line: the action doesn't read it",
                directory.exec_path
            )));
        };
        let within = tree_files(backend, *tree).await?;
        let paths = within.iter().map(|path| format!("{}/{path}", directory.exec_path)).collect();
        files.insert(directory.exec_path, paths);
    }
    let lookup = |path: PathId| files.get(&path).cloned().ok_or_else(|| format!("directory '{path}' wasn't listed"));
    command_line::expand(arguments, directories, &lookup).map_err(SkyError::new)
}

/// The paths of a `Tree`'s files and symlinks, relative to its root, in
/// order.
async fn tree_files(backend: &Backend, tree: BlobDigest) -> Result<Vec<String>, SkyError> {
    let bytes = backend.cas.read_bytes(&tree.to_cas()).await.map_err(cas_error)?;
    let tree = reapi::Tree::decode_from_slice(&bytes).map_err(|error| SkyError::new(format!("invalid Tree {tree:?}: {error}")))?;
    let mut children = HashMap::new();
    for child in &tree.children {
        children.insert(digest_of(child), child);
    }
    let mut files = Vec::new();
    let root = tree.root.as_option().cloned().unwrap_or_default();
    let mut pending = vec![(String::new(), root)];
    while let Some((prefix, directory)) = pending.pop() {
        for name in directory.files.iter().map(|file| &file.name).chain(directory.symlinks.iter().map(|symlink| &symlink.name)) {
            files.push(format!("{prefix}{name}"));
        }
        for node in &directory.directories {
            let digest = node.digest.as_option().map(|digest| digest.hash.clone()).unwrap_or_default();
            let Some(child) = children.get(&digest) else {
                return Err(SkyError::new(format!("Tree lacks directory '{prefix}{}'", node.name)));
            };
            pending.push((format!("{prefix}{}/", node.name), (*child).clone()));
        }
    }
    files.sort();
    Ok(files)
}

/// A `Directory`'s hash, which its parent names it by.
fn digest_of(directory: &reapi::Directory) -> String {
    cas::Digest::of_bytes(&directory.encode_to_vec()).hash().to_string()
}
