//! The digests of source files.

use crate::digest::BlobDigest;
use bazel_fs::{FileChanges, RootedPath};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A source file's digest, or `None` if it isn't a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SourceDigest {
    pub digest: Option<BlobDigest>,
    pub executable: bool,
}

encoded_value!(SourceDigest);

/// Hashes a source file, as actions that read it need. Actions upload the
/// sources they read that the CAS doesn't have, in one batch per action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SourceDigestKey(pub RootedPath);

impl Key for SourceDigestKey {
    type Value = SourceDigest;
    const KIND: &'static str = "SOURCE_DIGEST";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<SourceDigest, SkyError> {
        let path = self.0.resolve(ctx)?;
        let changes = ctx.global::<FileChanges>().ok();
        let rooted = self.0;
        let hashed_path = path.clone();
        let span = tracing::info_span!("hash_source");
        let result = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            if let Some(changes) = changes {
                changes.record(rooted, &hashed_path);
            }
            hash_file(&hashed_path)
        })
        .await;
        match result {
            Ok(result) => result,
            Err(error) => Err(SkyError::transient(format!("hashing task failed: {error}"))),
        }
    }
}

fn hash_file(path: &Path) -> Result<SourceDigest, SkyError> {
    let missing = SourceDigest {
        digest: None,
        executable: false,
    };
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(missing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(missing),
        Err(error) => return Err(io_error(path, error)),
    };
    let mut file = std::fs::File::open(path).map_err(|error| io_error(path, error))?;
    let mut hasher = cas::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| io_error(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(SourceDigest {
        digest: Some(BlobDigest::from_cas(&hasher.finish())),
        executable: metadata.permissions().mode() & 0o111 != 0,
    })
}

fn io_error(path: &Path, error: std::io::Error) -> SkyError {
    SkyError::transient(format!("failed to read {}: {error}", path.display()))
}
