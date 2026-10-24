//! Where Bzlmod fetches from and keeps what it fetches.

use bazel_fetch::DownloadCache;
use std::path::PathBuf;

/// The default registry.
pub const BCR: &str = "https://bcr.bazel.build";

/// A graph service: the registries to resolve modules from, and the
/// machine-wide directories downloads, registry files and fetched
/// repositories are kept in, shared by every workspace.
pub struct BzlmodConfig {
    pub registries: Vec<String>,
    pub downloads: DownloadCache,
    pub registry_cache: PathBuf,
    pub repos_dir: PathBuf,
    /// Bazel's built-in `@bazel_tools`.
    pub bazel_tools: Option<PathBuf>,
}

impl BzlmodConfig {
    /// A configuration keeping downloads in `repository_cache`, and the rest
    /// beneath `dir`.
    pub fn new(dir: PathBuf, repository_cache: PathBuf, bazel_tools: Option<PathBuf>) -> BzlmodConfig {
        BzlmodConfig {
            registries: vec![BCR.to_string()],
            downloads: DownloadCache::new(repository_cache),
            registry_cache: dir.join("registry_cache"),
            repos_dir: dir.join("repos"),
            bazel_tools,
        }
    }
}
