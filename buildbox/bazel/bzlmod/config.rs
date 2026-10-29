//! Where Bzlmod fetches from and keeps what it fetches.

use crate::repo_env::ClientEnv;
use bazel_fetch::{CredentialHelpers, DownloadCache, Netrc};
use skyframe::{Ctx, SkyError};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The default registry.
pub const BCR: &str = "https://bcr.bazel.build";

/// A graph service: the machine-wide directories downloads, registry files
/// and fetched repositories are kept in, shared by every workspace.
pub struct BzlmodConfig {
    pub downloads: DownloadCache,
    pub registry_cache: PathBuf,
    pub repos_dir: PathBuf,
    /// Bazel's built-in `@bazel_tools`.
    pub bazel_tools: Option<PathBuf>,
}

impl BzlmodConfig {
    /// The download cache, sending the headers the latest command's
    /// credential helpers give, or the credentials of the `.netrc` file of
    /// its environment, as Bazel's downloads do.
    pub fn downloads_for(&self, ctx: &Ctx) -> Result<DownloadCache, SkyError> {
        let env = ctx.global::<ClientEnv>()?.current();
        let netrc = Netrc::load(|name| env.get(name).map(str::to_string));
        let downloads = self.downloads.with_netrc(netrc);
        match ctx.global::<CommandCredentials>() {
            Ok(credentials) => Ok(downloads.with_credentials(credentials.current())),
            Err(_) => Ok(downloads),
        }
    }

    /// A configuration keeping downloads in `repository_cache`, and the rest
    /// beneath `dir`.
    pub fn new(dir: PathBuf, repository_cache: PathBuf, bazel_tools: Option<PathBuf>) -> BzlmodConfig {
        BzlmodConfig {
            downloads: DownloadCache::new(repository_cache),
            registry_cache: dir.join("registry_cache"),
            repos_dir: dir.join("repos"),
            bazel_tools,
        }
    }
}

/// A graph service: the credential helpers of the latest command. They
/// decide how downloads authenticate, not what they download, so changing
/// them invalidates nothing.
#[derive(Debug, Default)]
pub struct CommandCredentials(Mutex<Arc<CredentialHelpers>>);

impl CommandCredentials {
    pub fn new() -> Arc<CommandCredentials> {
        Arc::default()
    }

    pub fn set(&self, helpers: CredentialHelpers) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(helpers);
    }

    pub fn current(&self) -> Arc<CredentialHelpers> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}
