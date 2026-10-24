//! Command-line flags that override configuration settings.

use clap::Args;
use config::{Config, ConfigError, Environment, Settings};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// The config file to read. Defaults to
    /// $XDG_CONFIG_HOME/buildbox/config.toml, if it exists.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Run the server with this name, which keeps its storage apart from
    /// other servers' and listens on a port of its own. Defaults to
    /// $BB_SERVER_NAME, or the default server.
    #[arg(long = "server-name", value_name = "NAME")]
    pub name: Option<String>,

    /// The address to listen on.
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<SocketAddr>,

    /// The directory holding the CAS, action cache, sandboxes and artifacts,
    /// unless they are set individually.
    #[arg(long, value_name = "DIR")]
    pub root: Option<PathBuf>,

    /// Where to store blobs.
    #[arg(long, value_name = "DIR")]
    pub cas_dir: Option<PathBuf>,

    /// Where to store action results.
    #[arg(long, value_name = "DIR")]
    pub action_cache_dir: Option<PathBuf>,

    /// Where to run actions.
    #[arg(long, value_name = "DIR")]
    pub sandbox_dir: Option<PathBuf>,

    /// Where to store artifacts, such as container images.
    #[arg(long, value_name = "DIR")]
    pub artifacts_dir: Option<PathBuf>,

    /// How many actions may run at once. Defaults to the number of CPUs.
    #[arg(long, short = 'j', value_name = "N")]
    pub max_concurrent_actions: Option<usize>,

    /// Keep sandboxes after actions finish, for debugging.
    #[arg(long)]
    pub keep_sandboxes: bool,

    /// Append a trace of everything the server does to this file, as OTLP
    /// JSON.
    #[arg(long, value_name = "PATH")]
    pub trace_file: Option<PathBuf>,
}

impl ConfigArgs {
    pub fn load(&self) -> Result<Config, ConfigError> {
        let overrides = Settings {
            server_name: self.name.clone(),
            listen: self.listen,
            root: self.root.clone(),
            cas_dir: self.cas_dir.clone(),
            action_cache_dir: self.action_cache_dir.clone(),
            sandbox_dir: self.sandbox_dir.clone(),
            artifacts_dir: self.artifacts_dir.clone(),
            bazel_dir: None,
            max_concurrent_actions: self.max_concurrent_actions,
            keep_sandboxes: self.keep_sandboxes.then_some(true),
            trace_file: self.trace_file.clone(),
            // Environments and remote services can only be set in the
            // config file.
            environments: None,
            remote_executor: None,
            remote_cache: None,
            remote_instance_name: None,
        };
        config::load(self.config.as_deref(), overrides, &Environment::from_process())
    }
}
