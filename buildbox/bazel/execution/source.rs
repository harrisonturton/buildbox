//! The digests of source files.

use crate::action_key::cas_error;
use crate::backend::Backend;
use crate::digest::BlobDigest;
use bazel_fs::{FileChanges, RootedPath};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::io::Read;
use tracing::Instrument;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A source file's digest, or `None` if it isn't a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SourceDigest {
    pub digest: Option<BlobDigest>,
    pub executable: bool,
}

encoded_value!(SourceDigest);

/// Hashes a source file, as actions that read it need, and stores it in
/// the CAS actions read their inputs from. Every action that reads the file
/// shares the one upload.
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
        let digest = match result {
            Ok(result) => result?,
            Err(error) => return Err(SkyError::transient(format!("hashing task failed: {error}"))),
        };
        if let (Some(blob), Ok(backend)) = (digest.digest, ctx.global::<Backend>()) {
            store(&backend, blob, &path).instrument(tracing::info_span!("store_source")).await?;
        }
        Ok(digest)
    }
}

/// Put a source file in the CAS, unless it's there already.
async fn store(backend: &Backend, blob: BlobDigest, path: &Path) -> Result<(), SkyError> {
    let digest = blob.to_cas();
    let missing = backend.cas.find_missing(std::slice::from_ref(&digest)).await.map_err(cas_error)?;
    if missing.is_empty() {
        return Ok(());
    }
    let stored = backend.cas.put_file(path).await.map_err(cas_error)?;
    if stored != digest {
        return Err(SkyError::transient(format!("{} changed while it was read", path.display())));
    }
    Ok(())
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
