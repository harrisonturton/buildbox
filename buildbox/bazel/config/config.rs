//! Build configurations: the options a target is built with.
//!
//! A target can be built in several configurations in one build: for the
//! target platform, and for the execution platform when it's a tool another
//! target uses. Each configuration is interned, so configured targets refer
//! to theirs by a 4-byte [`ConfigId`].

use bazel_label::Label;
use intern::BlobTable;
use skycodec::{Decode, Encode, Fingerprinter};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

/// The options that decide how targets are built.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BuildOptions {
    /// `fastbuild`, `dbg` or `opt`.
    pub compilation_mode: String,
    /// The target CPU, as Bazel names it: `k8`, `aarch64`.
    pub cpu: String,
    /// The platform targets are built for.
    pub platform: Label,
    /// The platform tools run on.
    pub host_platform: Label,
    /// Whether this is an execution configuration, for tools.
    pub is_exec: bool,
    /// `--define` values, sorted by name.
    pub defines: Vec<(String, String)>,
    /// Every other option that affects analysis, as given, sorted: what
    /// `config_setting(values = ...)` matches against.
    pub values: Vec<(String, String)>,
    /// Starlark flags, `--//pkg:flag=value`, sorted by label.
    pub starlark_flags: Vec<(Label, String)>,
}

/// Identifies an interned configuration.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ConfigId(u32);

/// The configurations of this process, by id.
static CONFIGS: LazyLock<Configs> = LazyLock::new(Configs::default);

#[derive(Default)]
struct Configs {
    table: BlobTable,
    decoded: Mutex<Vec<Option<Arc<BuildConfiguration>>>>,
}

/// A configuration: its options, and the names derived from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildConfiguration {
    pub id: ConfigId,
    pub options: BuildOptions,
    /// The name of its output directory under `bazel-out`, such as
    /// `k8-fastbuild` or `k8-opt-exec-ST-d57f47055a04`.
    pub output_directory: String,
    /// A short hash of the options, as `cquery` prints it.
    pub checksum: String,
}

impl ConfigId {
    /// Intern a configuration.
    pub fn intern(options: &BuildOptions) -> ConfigId {
        ConfigId(CONFIGS.table.intern(&skycodec::to_bytes(options)))
    }

    /// The configuration with this id.
    pub fn get(self) -> Arc<BuildConfiguration> {
        let mut decoded = CONFIGS.decoded.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = self.0 as usize;
        if decoded.len() <= index {
            decoded.resize(index + 1, None);
        }
        if let Some(config) = &decoded[index] {
            return config.clone();
        }
        let options: BuildOptions = skycodec::from_bytes(CONFIGS.table.bytes(self.0)).expect("interned options decode");
        let config = Arc::new(BuildConfiguration::new(self, options));
        decoded[index] = Some(config.clone());
        config
    }

    pub fn id(self) -> u32 {
        self.0
    }
}

impl fmt::Debug for ConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get().checksum)
    }
}

impl BuildConfiguration {
    fn new(id: ConfigId, options: BuildOptions) -> BuildConfiguration {
        let mut fingerprinter = Fingerprinter::new();
        fingerprinter.bytes(format!("{options:?}").as_bytes());
        let digest = fingerprinter.finish().to_string();
        let output_directory = if options.is_exec {
            format!("{}-{}-exec-ST-{}", options.cpu, options.compilation_mode, &digest[..12])
        } else if options.starlark_flags.is_empty() {
            format!("{}-{}", options.cpu, options.compilation_mode)
        } else {
            format!("{}-{}-ST-{}", options.cpu, options.compilation_mode, &digest[..12])
        };
        BuildConfiguration {
            id,
            options,
            output_directory,
            checksum: digest[..7].to_string(),
        }
    }

    /// The directory outputs of `repo`'s packages go in, relative to the
    /// execution root, such as `bazel-out/k8-fastbuild/bin`.
    pub fn bin_dir(&self) -> String {
        format!("bazel-out/{}/bin", self.output_directory)
    }

    /// The value an option has, for `config_setting(values = ...)`.
    pub fn value(&self, name: &str) -> Option<&str> {
        match name {
            "compilation_mode" => Some(&self.options.compilation_mode),
            "cpu" => Some(&self.options.cpu),
            _ => self.options.values.iter().find(|(option, _)| option == name).map(|(_, value)| value.as_str()),
        }
    }

    /// The value of a `--define`.
    pub fn define(&self, name: &str) -> Option<&str> {
        self.options.defines.iter().find(|(define, _)| define == name).map(|(_, value)| value.as_str())
    }

    /// The configuration tools of targets in this one are built in: the
    /// execution platform's, optimised, as Bazel's exec transition gives.
    pub fn exec(&self) -> ConfigId {
        let mut options = self.options.clone();
        options.is_exec = true;
        options.compilation_mode = "opt".to_string();
        options.platform = options.host_platform;
        ConfigId::intern(&options)
    }
}

impl BuildOptions {
    /// The options Bazel uses by default on this machine, targeting the host.
    pub fn host_default(host_platform: Label) -> BuildOptions {
        BuildOptions {
            compilation_mode: "fastbuild".to_string(),
            cpu: host_cpu().to_string(),
            platform: host_platform,
            host_platform,
            is_exec: false,
            defines: Vec::new(),
            values: Vec::new(),
            starlark_flags: Vec::new(),
        }
    }
}

/// This machine's CPU, as Bazel's `--cpu` names it.
pub fn host_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "k8",
        "aarch64" => if cfg!(target_os = "macos") { "darwin_arm64" } else { "aarch64" },
        "arm" => "arm",
        "powerpc64" => "ppc",
        "s390x" => "s390x",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> BuildOptions {
        BuildOptions::host_default(Label::parse_canonical("@@platforms//host").unwrap())
    }

    #[test]
    fn configurations_are_interned() {
        let a = ConfigId::intern(&options());
        let b = ConfigId::intern(&options());
        assert_eq!(a, b);
        assert_eq!(a.get().output_directory, format!("{}-fastbuild", host_cpu()));
        assert_eq!(a.get().bin_dir(), format!("bazel-out/{}-fastbuild/bin", host_cpu()));
    }

    #[test]
    fn exec_configurations_are_optimised_and_named_apart() {
        let target = ConfigId::intern(&options()).get();
        let exec = target.exec().get();
        assert!(exec.options.is_exec);
        assert_eq!(exec.value("compilation_mode"), Some("opt"));
        assert!(exec.output_directory.starts_with(&format!("{}-opt-exec-ST-", host_cpu())));
        assert_eq!(exec.exec(), target.exec());
    }
}
