//! Looking up cached action results that are safe to return to a client.

use crate::status;
use actioncache::ActionCache;
use buffa::Message;
use cas::{Cas, CasError, Digest, DigestError};
use connectrpc::ConnectError;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{ActionResult, Tree};

/// Look up the cached result of an action, returning it only if every output
/// it refers to is still in the CAS. Otherwise the client couldn't download
/// the outputs, so the result is treated as missing.
#[tracing::instrument(name = "check_action_cache", skip_all)]
pub async fn lookup(cas: &dyn Cas, action_cache: &dyn ActionCache, action: &Digest) -> Result<Option<ActionResult>, ConnectError> {
    let result = action_cache.get(action).await.map_err(status::from_action_cache_error)?;
    let Some(result) = result else {
        return Ok(None);
    };

    match outputs_present(cas, &result).await {
        Ok(true) => Ok(Some(result)),
        Ok(false) => {
            tracing::info!("ignoring cached result for {action}: some outputs are no longer in the CAS");
            Ok(None)
        }
        Err(OutputsError::InvalidDigest(error)) => {
            tracing::warn!("ignoring cached result for {action}: {error}");
            Ok(None)
        }
        Err(OutputsError::Cas(error)) => Err(status::from_cas_error(error)),
    }
}

enum OutputsError {
    InvalidDigest(DigestError),
    Cas(CasError),
}

impl From<DigestError> for OutputsError {
    fn from(error: DigestError) -> Self {
        OutputsError::InvalidDigest(error)
    }
}

impl From<CasError> for OutputsError {
    fn from(error: CasError) -> Self {
        OutputsError::Cas(error)
    }
}

async fn outputs_present(cas: &dyn Cas, result: &ActionResult) -> Result<bool, OutputsError> {
    let mut digests = vec![];

    for file in &result.output_files {
        digests.push(Digest::from_required_proto(file.digest.as_option())?);
    }
    let std_streams = [&result.stdout_digest, &result.stderr_digest];
    for proto in std_streams.into_iter().filter_map(|digest| digest.as_option()) {
        digests.push(Digest::from_proto(proto)?);
    }
    for directory in &result.output_directories {
        let tree_digest = Digest::from_required_proto(directory.tree_digest.as_option())?;
        let Some(tree) = read_tree(cas, &tree_digest).await? else {
            return Ok(false);
        };
        add_tree_files(&tree, &mut digests)?;
    }

    let missing = cas.find_missing(&digests).await?;
    Ok(missing.is_empty())
}

/// Read a `Tree`, or `None` if it's missing or unreadable.
async fn read_tree(cas: &dyn Cas, digest: &Digest) -> Result<Option<Tree>, OutputsError> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Tree::decode_from_slice(&data).ok())
}

fn add_tree_files(tree: &Tree, digests: &mut Vec<Digest>) -> Result<(), OutputsError> {
    let directories = tree.root.as_option().into_iter().chain(&tree.children);
    for directory in directories {
        for file in &directory.files {
            digests.push(Digest::from_required_proto(file.digest.as_option())?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use actioncache_mem::MemActionCache;
    use cas_mem::MemCas;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::{Directory, FileNode, OutputDirectory, OutputFile};

    fn result_with_output(digest: &Digest) -> ActionResult {
        ActionResult {
            output_files: vec![OutputFile {
                path: "out".to_string(),
                digest: digest.to_proto().into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn returns_results_whose_outputs_are_present() {
        let cas = MemCas::new();
        let cache = MemActionCache::new();
        let action = Digest::of_bytes(b"action");
        let output = cas.put_bytes(b"output").await.unwrap();
        cache.put(&action, &result_with_output(&output)).await.unwrap();

        let result = lookup(&cas, &cache, &action).await.unwrap();

        assert_eq!(result, Some(result_with_output(&output)));
    }

    #[tokio::test]
    async fn misses_when_nothing_is_cached() {
        let cas = MemCas::new();
        let cache = MemActionCache::new();

        let result = lookup(&cas, &cache, &Digest::of_bytes(b"action")).await.unwrap();

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn misses_when_an_output_file_is_gone() {
        let cas = MemCas::new();
        let cache = MemActionCache::new();
        let action = Digest::of_bytes(b"action");
        let gone = Digest::of_bytes(b"gone");
        cache.put(&action, &result_with_output(&gone)).await.unwrap();

        let result = lookup(&cas, &cache, &action).await.unwrap();

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn misses_when_a_file_inside_an_output_directory_is_gone() {
        let cas = MemCas::new();
        let cache = MemActionCache::new();
        let action = Digest::of_bytes(b"action");
        let tree = Tree {
            root: Directory {
                files: vec![FileNode {
                    name: "gone".to_string(),
                    digest: Digest::of_bytes(b"gone").to_proto().into(),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let tree_digest = cas.put_bytes(&tree.encode_to_vec()).await.unwrap();
        let result = ActionResult {
            output_directories: vec![OutputDirectory {
                path: "dir".to_string(),
                tree_digest: tree_digest.to_proto().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        cache.put(&action, &result).await.unwrap();

        let result = lookup(&cas, &cache, &action).await.unwrap();

        assert_eq!(result, None);
    }
}
