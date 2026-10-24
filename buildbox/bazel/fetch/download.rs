//! Downloading files into a content-addressed cache.

use crate::integrity::{HashAlgorithm, Hasher, Integrity};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("no URLs to download from")]
    NoUrls,

    #[error("failed to download {url}: {message}")]
    Http { url: String, message: String },

    #[error("checksum mismatch for {url}: expected {expected}, got {actual}")]
    Checksum { url: String, expected: Integrity, actual: Integrity },

    #[error("{context}: {error}")]
    Io { context: String, error: std::io::Error },

    #[error("{0}")]
    All(String),
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> DownloadError {
    let context = context.into();
    move |error| DownloadError::Io { context, error }
}

/// Files downloaded before, by SHA-256, laid out as Bazel's repository
/// cache is: `content_addressable/sha256/<hash>/file`.
#[derive(Clone, Debug)]
pub struct DownloadCache {
    dir: PathBuf,
}

impl DownloadCache {
    pub fn new(dir: impl Into<PathBuf>) -> DownloadCache {
        DownloadCache { dir: dir.into() }
    }

    fn entry(&self, sha256: &str) -> PathBuf {
        self.dir.join("content_addressable/sha256").join(sha256).join("file")
    }

    /// Download the file at the first of `urls` that works to `dest`,
    /// verifying it against `integrity` if given, and return its checksum.
    /// A file already in the cache isn't downloaded again. Blocks.
    pub fn download(&self, urls: &[String], integrity: Option<&Integrity>, dest: &Path) -> Result<Integrity, DownloadError> {
        if let Some(sha256) = integrity.and_then(Integrity::sha256) {
            let cached = self.entry(&sha256);
            if cached.is_file() {
                copy_or_link(&cached, dest)?;
                return Ok(integrity.cloned().expect("checked above"));
            }
        }
        if urls.is_empty() {
            return Err(DownloadError::NoUrls);
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io(format!("failed to create {}", parent.display())))?;
        }

        let mut failures = Vec::new();
        for url in urls {
            match self.download_one(url, integrity, dest) {
                Ok(result) => return Ok(result),
                Err(error @ DownloadError::Checksum { .. }) => return Err(error),
                Err(error) => failures.push(error.to_string()),
            }
        }
        Err(DownloadError::All(failures.join("; ")))
    }

    fn download_one(&self, url: &str, integrity: Option<&Integrity>, dest: &Path) -> Result<Integrity, DownloadError> {
        let _span = tracing::info_span!("fetch.download", url = %url).entered();
        let algorithm = integrity.map(Integrity::algorithm).unwrap_or(HashAlgorithm::Sha256);
        let partial = dest.with_extension("partial");
        let mut file = File::create(&partial).map_err(io(format!("failed to create {}", partial.display())))?;
        let (sha256, actual) = fetch_into(url, &mut file, algorithm)?;
        drop(file);

        if let Some(expected) = integrity {
            if &actual != expected {
                let _ = std::fs::remove_file(&partial);
                return Err(DownloadError::Checksum {
                    url: url.to_string(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        std::fs::rename(&partial, dest).map_err(io(format!("failed to move download to {}", dest.display())))?;
        self.store(&sha256, dest);
        Ok(actual)
    }

    /// Keep a copy of a downloaded file in the cache. Failing to is only a
    /// lost optimization.
    fn store(&self, sha256: &Integrity, file: &Path) {
        let Some(hash) = sha256.sha256() else {
            return;
        };
        let entry = self.entry(&hash);
        if entry.exists() {
            return;
        }
        let Some(parent) = entry.parent() else {
            return;
        };
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
        let temporary = parent.join(format!("file.{}.tmp", std::process::id()));
        if std::fs::copy(file, &temporary).is_ok() {
            let _ = std::fs::rename(&temporary, &entry);
        }
    }
}

/// Download `url` into `out`, returning its SHA-256 and its checksum with
/// `algorithm`.
fn fetch_into(url: &str, out: &mut File, algorithm: HashAlgorithm) -> Result<(Integrity, Integrity), DownloadError> {
    if let Some(path) = url.strip_prefix("file://") {
        let mut input = File::open(path).map_err(io(format!("failed to open {path}")))?;
        return copy_hashing(&mut input, out, algorithm, url);
    }
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .try_proxy_from_env(true)
        .build();
    let response = agent.get(url).call().map_err(|error| DownloadError::Http {
        url: url.to_string(),
        message: error.to_string(),
    })?;
    let mut reader = response.into_reader();
    copy_hashing(&mut reader, out, algorithm, url)
}

fn copy_hashing(input: &mut dyn Read, out: &mut File, algorithm: HashAlgorithm, url: &str) -> Result<(Integrity, Integrity), DownloadError> {
    let mut sha256 = Hasher::new(HashAlgorithm::Sha256);
    let mut checksum = Hasher::new(algorithm);
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(|error| DownloadError::Http {
            url: url.to_string(),
            message: error.to_string(),
        })?;
        if read == 0 {
            break;
        }
        sha256.update(&buffer[..read]);
        checksum.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(io("failed to write download"))?;
    }
    Ok((sha256.finish(), checksum.finish()))
}

fn copy_or_link(from: &Path, to: &Path) -> Result<(), DownloadError> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(io(format!("failed to create {}", parent.display())))?;
    }
    let _ = std::fs::remove_file(to);
    if std::fs::hard_link(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(io(format!("failed to copy {} to {}", from.display(), to.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_file_urls_and_caches_them() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        std::fs::write(&source, "hello").unwrap();
        let cache = DownloadCache::new(dir.path().join("cache"));
        let expected = Integrity::sha256_hex("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap();

        let url = format!("file://{}", source.display());
        let first = dir.path().join("out/first.txt");
        assert_eq!(cache.download(&[url], Some(&expected), &first).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "hello");

        std::fs::remove_file(&source).unwrap();
        let second = dir.path().join("out/second.txt");
        let missing = format!("file://{}", source.display());
        assert_eq!(cache.download(&[missing], Some(&expected), &second).unwrap(), expected);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "hello");
    }

    #[test]
    fn checksum_mismatches_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.txt");
        std::fs::write(&source, "not hello").unwrap();
        let cache = DownloadCache::new(dir.path().join("cache"));
        let expected = Integrity::sha256_hex("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824").unwrap();
        let url = format!("file://{}", source.display());
        let error = cache.download(&[url], Some(&expected), &dir.path().join("out")).unwrap_err();
        assert!(matches!(error, DownloadError::Checksum { .. }), "{error}");
    }
}
