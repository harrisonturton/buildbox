//! The input trees of actions, as REAPI `Directory` messages.

use crate::digest::BlobDigest;
use buffa::Message;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{Directory, DirectoryNode, FileNode};
use std::collections::BTreeMap;

/// A tree of files being built up, which becomes a Merkle tree of
/// `Directory` messages.
#[derive(Default)]
pub struct InputTree {
    root: Dir,
}

#[derive(Default)]
struct Dir {
    files: BTreeMap<String, (BlobDigest, bool)>,
    dirs: BTreeMap<String, Dir>,
    /// Directories already in the CAS, by the digest of their `Directory`.
    subtrees: BTreeMap<String, BlobDigest>,
}

/// The digest of the root `Directory`, and every `Directory` blob in the
/// tree.
pub struct MerkleTree {
    pub root: cas::Digest,
    pub blobs: Vec<(cas::Digest, Vec<u8>)>,
}

impl InputTree {
    /// Add a file at a path relative to the input root.
    pub fn add_file(&mut self, path: &str, digest: BlobDigest, executable: bool) -> Result<(), String> {
        let (dir, name) = self.parent(path)?;
        if dir.dirs.contains_key(name) || dir.subtrees.contains_key(name) {
            return Err(format!("input '{path}' is both a file and a directory"));
        }
        if let Some((existing, _)) = dir.files.get(name) {
            if *existing != digest {
                return Err(format!("input '{path}' is given twice, with different contents"));
            }
        }
        dir.files.insert(name.to_string(), (digest, executable));
        Ok(())
    }

    /// Add a directory whose `Directory` message, and those beneath it, are
    /// already in the CAS.
    pub fn add_directory(&mut self, path: &str, digest: BlobDigest) -> Result<(), String> {
        let (dir, name) = self.parent(path)?;
        if dir.files.contains_key(name) {
            return Err(format!("input '{path}' is both a file and a directory"));
        }
        dir.subtrees.insert(name.to_string(), digest);
        Ok(())
    }

    fn parent<'a, 'p>(&'a mut self, path: &'p str) -> Result<(&'a mut Dir, &'p str), String> {
        let mut segments: Vec<&str> = path.split('/').collect();
        let Some(name) = segments.pop().filter(|name| !name.is_empty()) else {
            return Err(format!("invalid input path '{path}'"));
        };
        let mut dir = &mut self.root;
        for segment in segments {
            if dir.files.contains_key(segment) {
                return Err(format!("input '{path}' is beneath a file"));
            }
            dir = dir.dirs.entry(segment.to_string()).or_default();
        }
        Ok((dir, name))
    }

    pub fn finish(self) -> MerkleTree {
        let mut blobs = Vec::new();
        let root = encode(self.root, &mut blobs);
        MerkleTree { root, blobs }
    }
}

/// Encode a directory and those beneath it, returning its digest.
fn encode(dir: Dir, blobs: &mut Vec<(cas::Digest, Vec<u8>)>) -> cas::Digest {
    let files = dir
        .files
        .into_iter()
        .map(|(name, (digest, executable))| FileNode {
            name,
            digest: digest.to_cas().to_proto().into(),
            is_executable: executable,
            ..Default::default()
        })
        .collect();
    let mut directories: Vec<DirectoryNode> = dir
        .dirs
        .into_iter()
        .map(|(name, child)| DirectoryNode {
            name,
            digest: encode(child, blobs).to_proto().into(),
            ..Default::default()
        })
        .collect();
    directories.extend(dir.subtrees.into_iter().map(|(name, digest)| DirectoryNode {
        name,
        digest: digest.to_cas().to_proto().into(),
        ..Default::default()
    }));
    directories.sort_by(|a, b| a.name.cmp(&b.name));
    let directory = Directory {
        files,
        directories,
        ..Default::default()
    };
    let bytes = directory.encode_to_vec();
    let digest = cas::Digest::of_bytes(&bytes);
    blobs.push((digest.clone(), bytes));
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> BlobDigest {
        BlobDigest::from_cas(&cas::Digest::of_bytes(data))
    }

    #[test]
    fn builds_nested_directories() {
        let mut tree = InputTree::default();
        tree.add_file("a/b/c.txt", digest(b"c"), false).unwrap();
        tree.add_file("a/d.txt", digest(b"d"), true).unwrap();
        tree.add_file("e.txt", digest(b"e"), false).unwrap();
        let merkle = tree.finish();
        // a/b, a, and the root.
        assert_eq!(merkle.blobs.len(), 3);
        assert_eq!(merkle.blobs.last().unwrap().0, merkle.root);
    }

    #[test]
    fn rejects_conflicts() {
        let mut tree = InputTree::default();
        tree.add_file("a", digest(b"a"), false).unwrap();
        assert!(tree.add_file("a/b", digest(b"b"), false).is_err());
        assert!(tree.add_file("a", digest(b"other"), false).is_err());
        assert!(tree.add_file("a", digest(b"a"), false).is_ok());
    }
}
