//! The digests of source files.

use crate::digest::BlobDigest;
use bazel_fs::{FileChanges, RootedPath};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::io::Read;
use persist::PersistedMap;
use skycodec::{Fingerprint, Fingerprinter};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
///
/// Digests are kept across restarts in [`SourceDigests`], by the file's
/// identity, so an unchanged file isn't read again.
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
        let saved = ctx.global::<SourceDigests>().ok();
        let span = tracing::info_span!("hash_source");
        let result = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            if let Some(changes) = changes {
                changes.record(rooted, &hashed_path);
            }
            digest_file(&hashed_path, saved.as_deref())
        })
        .await;
        match result {
            Ok(result) => result,
            Err(error) => Err(SkyError::transient(format!("hashing task failed: {error}"))),
        }
    }
}

/// Digests of source files, kept across restarts, by the file's path and
/// what `stat` says of it.
pub struct SourceDigests(pub PersistedMap);

/// How long after a file last changed its digest may be kept: a change
/// within the timestamps' resolution of being hashed could otherwise look
/// like no change.
const SETTLED: Duration = Duration::from_secs(2);

fn digest_file(path: &Path, saved: Option<&SourceDigests>) -> Result<SourceDigest, SkyError> {
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
    let identity = identity(path, &metadata);
    if let Some(digest) = saved.and_then(|saved| saved.0.get(&identity)).and_then(|bytes| skycodec::from_bytes(&bytes).ok()) {
        return Ok(digest);
    }
    let digest = hash_file(path, &metadata)?;
    if let Some(saved) = saved.filter(|_| settled(&metadata)) {
        saved.0.insert(identity, &skycodec::to_bytes(&digest));
    }
    Ok(digest)
}

/// Identifies a file's contents without reading them, as far as `stat` can.
fn identity(path: &Path, metadata: &std::fs::Metadata) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(path.as_os_str().as_bytes());
    for field in [metadata.dev(), metadata.ino(), metadata.size(), metadata.mode() as u64] {
        fingerprinter.u64(field);
    }
    for field in [metadata.mtime(), metadata.mtime_nsec(), metadata.ctime(), metadata.ctime_nsec()] {
        fingerprinter.u64(field as u64);
    }
    fingerprinter.finish()
}

fn settled(metadata: &std::fs::Metadata) -> bool {
    let changed = UNIX_EPOCH + Duration::new(metadata.ctime().max(0) as u64, metadata.ctime_nsec().max(0) as u32);
    SystemTime::now().duration_since(changed).is_ok_and(|age| age > SETTLED)
}

fn hash_file(path: &Path, metadata: &std::fs::Metadata) -> Result<SourceDigest, SkyError> {
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
