//! Server configuration.
//!
//! Settings come from three sources, each overriding the one before:
//! built-in defaults, an optional TOML config file, and command-line flags.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

const DEFAULT_PORT: u16 = 50051;
const CONFIG_FILE_NAME: &str = "config.toml";

/// The fully resolved configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Config {
    /// The address the server listens on.
    pub listen: SocketAddr,

    /// Where blobs are stored.
    pub cas_dir: PathBuf,

    /// Where action results are stored.
    pub action_cache_dir: PathBuf,

    /// Where actions run, each in its own subdirectory.
    pub sandbox_dir: PathBuf,

    /// Where artifacts, such as container images, are stored.
    pub artifacts_dir: PathBuf,

    /// Where the Bazel-compatible commands keep fetched repositories,
    /// downloads and build outputs.
    pub bazel_dir: PathBuf,

    /// How many actions may run at once.
    pub max_concurrent_actions: usize,

    /// Whether to keep sandboxes after actions finish, for debugging.
    pub keep_sandboxes: bool,

    /// Where to append spans, as OTLP JSON. `None` means spans aren't
    /// exported.
    pub trace_file: Option<PathBuf>,

    /// The environments actions can run in, by name.
    pub environments: BTreeMap<String, ExecutionEnvironment>,
}

/// Where and how actions run. Actions choose an environment by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionEnvironment {
    /// Directly on this machine, with no isolation.
    Local,

    /// In a container, made from a container image in the artifact store.
    Container {
        /// The image's name in the artifact store.
        image: String,

        /// Whether actions can reach the network. Without it, they only
        /// have a loopback interface of their own.
        #[serde(default)]
        network: bool,
    },
}

/// Settings that may be given in the config file or on the command line.
/// Unset settings fall back to the next source.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub listen: Option<SocketAddr>,

    /// The directory that holds the CAS, action cache, sandboxes and
    /// artifacts, unless their locations are set individually.
    pub root: Option<PathBuf>,

    pub cas_dir: Option<PathBuf>,
    pub action_cache_dir: Option<PathBuf>,
    pub sandbox_dir: Option<PathBuf>,
    pub artifacts_dir: Option<PathBuf>,
    pub bazel_dir: Option<PathBuf>,
    pub max_concurrent_actions: Option<usize>,
    pub keep_sandboxes: Option<bool>,
    pub trace_file: Option<PathBuf>,
    pub environments: Option<BTreeMap<String, ExecutionEnvironment>>,
}

impl Settings {
    /// Combine two sets of settings, preferring those in `overrides`.
    pub fn merge(self, overrides: Settings) -> Settings {
        Settings {
            listen: overrides.listen.or(self.listen),
            root: overrides.root.or(self.root),
            cas_dir: overrides.cas_dir.or(self.cas_dir),
            action_cache_dir: overrides.action_cache_dir.or(self.action_cache_dir),
            sandbox_dir: overrides.sandbox_dir.or(self.sandbox_dir),
            artifacts_dir: overrides.artifacts_dir.or(self.artifacts_dir),
            bazel_dir: overrides.bazel_dir.or(self.bazel_dir),
            max_concurrent_actions: overrides.max_concurrent_actions.or(self.max_concurrent_actions),
            keep_sandboxes: overrides.keep_sandboxes.or(self.keep_sandboxes),
            trace_file: overrides.trace_file.or(self.trace_file),
            environments: overrides.environments.or(self.environments),
        }
    }

    /// Make every path absolute: `~` is the home directory, and relative
    /// paths are relative to `base`.
    fn resolve_paths(self, base: &Path, env: &Environment) -> Result<Settings, ConfigError> {
        Ok(Settings {
            root: resolve_optional_path(self.root, base, env)?,
            cas_dir: resolve_optional_path(self.cas_dir, base, env)?,
            action_cache_dir: resolve_optional_path(self.action_cache_dir, base, env)?,
            sandbox_dir: resolve_optional_path(self.sandbox_dir, base, env)?,
            artifacts_dir: resolve_optional_path(self.artifacts_dir, base, env)?,
            bazel_dir: resolve_optional_path(self.bazel_dir, base, env)?,
            trace_file: resolve_optional_path(self.trace_file, base, env)?,
            ..self
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {}: {error}", path.display())]
    Read { path: PathBuf, error: std::io::Error },

    #[error("invalid config file {}: {error}", path.display())]
    Parse { path: PathBuf, error: toml::de::Error },

    #[error("max_concurrent_actions must be at least 1")]
    NoConcurrency,

    #[error("can't expand ~ in {}: HOME isn't set", path.display())]
    NoHome { path: PathBuf },

    #[error("can't choose a default {what}: HOME isn't set")]
    NoDefault { what: &'static str },

    #[error("invalid environment name {0:?}: use only letters, digits, '.', '_' and '-'")]
    InvalidEnvironmentName(String),
}

/// The parts of the process environment the configuration depends on.
#[derive(Clone, Debug)]
pub struct Environment {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub current_dir: PathBuf,
    pub available_parallelism: usize,
}

impl Environment {
    pub fn from_process() -> Environment {
        Environment {
            home: non_empty_env_path("HOME"),
            xdg_config_home: non_empty_env_path("XDG_CONFIG_HOME"),
            xdg_cache_home: non_empty_env_path("XDG_CACHE_HOME"),
            current_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            available_parallelism: std::thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }

    /// The config file read when none is given explicitly.
    pub fn default_config_path(&self) -> Option<PathBuf> {
        let config_home = self
            .xdg_config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".config")))?;
        Some(config_home.join("buildbox").join(CONFIG_FILE_NAME))
    }

    fn default_root(&self) -> Option<PathBuf> {
        let cache_home = self
            .xdg_cache_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".cache")))?;
        Some(cache_home.join("buildbox"))
    }
}

/// Load the configuration.
///
/// If `config_path` is given that file must exist. Otherwise the default
/// config file is read if it exists. `overrides`, typically from the command
/// line, take precedence over the file, and relative paths in them are
/// relative to the current directory.
pub fn load(config_path: Option<&Path>, overrides: Settings, env: &Environment) -> Result<Config, ConfigError> {
    let file_settings = match config_path {
        Some(path) => read_file(&env.current_dir.join(path), env)?,
        None => read_default_file(env)?,
    };

    let overrides = overrides.resolve_paths(&env.current_dir, env)?;
    resolve(file_settings.merge(overrides), env)
}

fn read_default_file(env: &Environment) -> Result<Settings, ConfigError> {
    match env.default_config_path() {
        Some(path) if path.exists() => read_file(&path, env),
        _ => Ok(Settings::default()),
    }
}

fn read_file(path: &Path, env: &Environment) -> Result<Settings, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|error| ConfigError::Read {
        path: path.to_path_buf(),
        error,
    })?;
    let settings: Settings = toml::from_str(&content).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        error,
    })?;

    let base = path.parent().unwrap_or(Path::new("/"));
    settings.resolve_paths(base, env)
}

/// Fill in defaults for anything not set.
fn resolve(settings: Settings, env: &Environment) -> Result<Config, ConfigError> {
    let max_concurrent_actions = settings.max_concurrent_actions.unwrap_or(env.available_parallelism);
    if max_concurrent_actions == 0 {
        return Err(ConfigError::NoConcurrency);
    }

    let environments = settings.environments.unwrap_or_default();
    if let Some(name) = environments.keys().find(|name| !is_valid_environment_name(name)) {
        return Err(ConfigError::InvalidEnvironmentName(name.clone()));
    }

    let root = settings.root.or_else(|| env.default_root());
    let root = root.as_deref();

    Ok(Config {
        listen: settings.listen.unwrap_or(default_listen()),
        cas_dir: dir_or_default(settings.cas_dir, root, "cas", "cas_dir")?,
        action_cache_dir: dir_or_default(settings.action_cache_dir, root, "ac", "action_cache_dir")?,
        sandbox_dir: dir_or_default(settings.sandbox_dir, root, "sandbox", "sandbox_dir")?,
        artifacts_dir: dir_or_default(settings.artifacts_dir, root, "artifacts", "artifacts_dir")?,
        bazel_dir: dir_or_default(settings.bazel_dir, root, "bazel", "bazel_dir")?,
        max_concurrent_actions,
        keep_sandboxes: settings.keep_sandboxes.unwrap_or(false),
        trace_file: settings.trace_file,
        environments,
    })
}

fn is_valid_environment_name(name: &str) -> bool {
    let is_allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    !name.is_empty() && name.chars().all(is_allowed)
}

/// Use `dir` if it was set, or otherwise the subdirectory `name` of `root`.
/// `setting` names the setting, for the error if neither is available.
fn dir_or_default(dir: Option<PathBuf>, root: Option<&Path>, name: &str, setting: &'static str) -> Result<PathBuf, ConfigError> {
    if let Some(dir) = dir {
        return Ok(dir);
    }
    match root {
        Some(root) => Ok(root.join(name)),
        None => Err(ConfigError::NoDefault { what: setting }),
    }
}

/// Listen on the IPv6 loopback address only: buildbox has no authentication,
/// so it shouldn't be reachable from other machines by default.
fn default_listen() -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), DEFAULT_PORT)
}

fn resolve_optional_path(path: Option<PathBuf>, base: &Path, env: &Environment) -> Result<Option<PathBuf>, ConfigError> {
    match path {
        Some(path) => Ok(Some(resolve_path(&path, base, env)?)),
        None => Ok(None),
    }
}

fn resolve_path(path: &Path, base: &Path, env: &Environment) -> Result<PathBuf, ConfigError> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(base.join(path));
    };
    let Some(home) = &env.home else {
        return Err(ConfigError::NoHome {
            path: path.to_path_buf(),
        });
    };
    Ok(home.join(rest))
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_env(dir: &Path) -> Environment {
        Environment {
            home: Some(dir.join("home")),
            xdg_config_home: None,
            xdg_cache_home: None,
            current_dir: dir.join("cwd"),
            available_parallelism: 8,
        }
    }

    fn write_default_config(env: &Environment, content: &str) {
        let path = env.default_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn defaults_use_the_cache_dir_and_all_cpus() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());

        let config = load(None, Settings::default(), &env).unwrap();

        let root = dir.path().join("home/.cache/buildbox");
        assert_eq!(
            config,
            Config {
                listen: "[::1]:50051".parse().unwrap(),
                cas_dir: root.join("cas"),
                action_cache_dir: root.join("ac"),
                sandbox_dir: root.join("sandbox"),
                artifacts_dir: root.join("artifacts"),
                bazel_dir: root.join("bazel"),
                max_concurrent_actions: 8,
                keep_sandboxes: false,
                trace_file: None,
                environments: BTreeMap::new(),
            }
        );
    }

    #[test]
    fn xdg_dirs_take_precedence_over_home() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = test_env(dir.path());
        env.xdg_cache_home = Some(dir.path().join("xdg-cache"));
        env.xdg_config_home = Some(dir.path().join("xdg-config"));
        write_default_config(&env, "keep_sandboxes = true");

        let config = load(None, Settings::default(), &env).unwrap();

        assert_eq!(config.cas_dir, dir.path().join("xdg-cache/buildbox/cas"));
        assert!(config.keep_sandboxes);
    }

    #[test]
    fn command_line_overrides_file_which_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        write_default_config(&env, "listen = \"127.0.0.1:1234\"\nmax_concurrent_actions = 4\n");
        let overrides = Settings {
            max_concurrent_actions: Some(2),
            ..Default::default()
        };

        let config = load(None, overrides, &env).unwrap();

        assert_eq!(config.listen, "127.0.0.1:1234".parse().unwrap());
        assert_eq!(config.max_concurrent_actions, 2);
    }

    #[test]
    fn root_moves_every_directory_unless_set_individually() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        let overrides = Settings {
            root: Some(PathBuf::from("/data/buildbox")),
            sandbox_dir: Some(PathBuf::from("/fast/sandbox")),
            ..Default::default()
        };

        let config = load(None, overrides, &env).unwrap();

        assert_eq!(config.cas_dir, PathBuf::from("/data/buildbox/cas"));
        assert_eq!(config.action_cache_dir, PathBuf::from("/data/buildbox/ac"));
        assert_eq!(config.sandbox_dir, PathBuf::from("/fast/sandbox"));
        assert_eq!(config.artifacts_dir, PathBuf::from("/data/buildbox/artifacts"));
    }

    #[test]
    fn paths_expand_home_and_resolve_relative_to_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        let config_path = dir.path().join("etc/buildbox.toml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "cas_dir = \"~/cas\"\naction_cache_dir = \"ac\"\ntrace_file = \"traces.jsonl\"\n",
        )
        .unwrap();
        let overrides = Settings {
            sandbox_dir: Some(PathBuf::from("sandbox")),
            ..Default::default()
        };

        let config = load(Some(&config_path), overrides, &env).unwrap();

        assert_eq!(config.cas_dir, dir.path().join("home/cas"));
        assert_eq!(config.action_cache_dir, dir.path().join("etc/ac"));
        assert_eq!(config.sandbox_dir, dir.path().join("cwd/sandbox"));
        assert_eq!(config.trace_file, Some(dir.path().join("etc/traces.jsonl")));
    }

    #[test]
    fn explicit_config_file_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());

        let err = load(Some(Path::new("/no/such/file.toml")), Settings::default(), &env).unwrap_err();

        assert!(matches!(err, ConfigError::Read { .. }), "{err}");
    }

    #[test]
    fn unknown_settings_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        write_default_config(&env, "cas_directory = \"/typo\"");

        let err = load(None, Settings::default(), &env).unwrap_err();

        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn zero_concurrency_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        let overrides = Settings {
            max_concurrent_actions: Some(0),
            ..Default::default()
        };

        let err = load(None, overrides, &env).unwrap_err();

        assert!(matches!(err, ConfigError::NoConcurrency), "{err}");
    }

    #[test]
    fn without_home_directories_must_be_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = test_env(dir.path());
        env.home = None;

        let err = load(None, Settings::default(), &env).unwrap_err();

        assert!(matches!(err, ConfigError::NoDefault { .. }), "{err}");
    }

    #[test]
    fn environments_are_read_from_the_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        write_default_config(&env, "[environments.default]\nkind = \"local\"\n");

        let config = load(None, Settings::default(), &env).unwrap();

        let expected = BTreeMap::from([("default".to_string(), ExecutionEnvironment::Local)]);
        assert_eq!(config.environments, expected);
    }

    #[test]
    fn invalid_environment_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        write_default_config(&env, "[environments.\"has space\"]\nkind = \"local\"\n");

        let err = load(None, Settings::default(), &env).unwrap_err();

        assert!(matches!(err, ConfigError::InvalidEnvironmentName(_)), "{err}");
    }

    #[test]
    fn unknown_environment_kinds_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        write_default_config(&env, "[environments.vm]\nkind = \"virtual_machine\"\n");

        let err = load(None, Settings::default(), &env).unwrap_err();

        assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    }

    #[test]
    fn container_environments_name_an_image() {
        let dir = tempfile::tempdir().unwrap();
        let env = test_env(dir.path());
        let content = "[environments.gcc]\nkind = \"container\"\nimage = \"gcc:13\"\n\n\
                       [environments.online]\nkind = \"container\"\nimage = \"gcc:13\"\nnetwork = true\n";
        write_default_config(&env, content);

        let config = load(None, Settings::default(), &env).unwrap();

        assert_eq!(
            config.environments["gcc"],
            ExecutionEnvironment::Container {
                image: "gcc:13".to_string(),
                network: false,
            }
        );
        assert_eq!(
            config.environments["online"],
            ExecutionEnvironment::Container {
                image: "gcc:13".to_string(),
                network: true,
            }
        );
    }
}
