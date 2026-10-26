//! Build configurations: the options a target is built with.
//!
//! A target can be built in several configurations in one build: for the
//! target platform, and for the execution platform when it's a tool another
//! target uses. Each configuration is interned, so configured targets refer
//! to theirs by a 4-byte [`ConfigId`].

mod options;

pub use options::{BuildOptions, LabelParser, NATIVE_OPTIONS, NativeOption, OptionDefault, OptionError, OptionType, OptionValue, command_line_options, native_option};

use bazel_label::Label;
use intern::BlobTable;
use skycodec::{Decode, Encode, Fingerprinter};
use std::fmt;
use std::sync::{Arc, LazyLock, Mutex};

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

/// A configuration: its options, and what's derived from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildConfiguration {
    pub id: ConfigId,
    pub options: BuildOptions,
    /// `fastbuild`, `dbg` or `opt`.
    pub compilation_mode: String,
    /// The target CPU, as Bazel names it: `k8`, `aarch64`.
    pub cpu: String,
    /// The platform targets are built for: the first of `--platforms`, or
    /// the host's.
    pub platform: Label,
    /// The platform tools run on.
    pub host_platform: Label,
    /// Whether this is an execution configuration, for tools.
    pub is_exec: bool,
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
        let compilation_mode = options.string("compilation_mode").to_string();
        let cpu = options.string("cpu").to_string();
        let host_platform = options.label("host_platform").unwrap_or(*DEFAULT_HOST_PLATFORM);
        let platform = options.labels("platforms").first().copied().unwrap_or(host_platform);
        let is_exec = options.bool("is exec configuration");
        let output_directory = if is_exec {
            format!("{cpu}-{compilation_mode}-exec-ST-{}", &digest[..12])
        } else if options.starlark_flags.is_empty() {
            format!("{cpu}-{compilation_mode}")
        } else {
            format!("{cpu}-{compilation_mode}-ST-{}", &digest[..12])
        };
        BuildConfiguration {
            id,
            options,
            compilation_mode,
            cpu,
            platform,
            host_platform,
            is_exec,
            output_directory,
            checksum: digest[..7].to_string(),
        }
    }

    /// The directory outputs of `repo`'s packages go in, relative to the
    /// execution root, such as `bazel-out/k8-fastbuild/bin`.
    pub fn bin_dir(&self) -> String {
        format!("bazel-out/{}/bin", self.output_directory)
    }

    /// The configuration tools of targets in this one are built in: the
    /// execution platform's, optimised, as Bazel's exec transition gives.
    pub fn exec(&self) -> ConfigId {
        let mut options = self.options.clone();
        let exec_options = [
            ("is exec configuration", OptionValue::Bool(true)),
            ("compilation_mode", OptionValue::String("opt".to_string())),
            ("platforms", OptionValue::Labels(vec![self.host_platform])),
        ];
        for (name, value) in exec_options {
            options.set(name, value).expect("exec options are native options");
        }
        ConfigId::intern(&options)
    }
}

/// The platform tools run on when `--host_platform` doesn't say.
static DEFAULT_HOST_PLATFORM: LazyLock<Label> = LazyLock::new(|| Label::parse_canonical("@@platforms//host:host").expect("a canonical label"));

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
        BuildOptions::default()
    }

    fn parse_label(text: &str) -> Result<Label, String> {
        Label::parse_canonical(text).map_err(|error| error.to_string())
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
        assert!(exec.is_exec);
        assert_eq!(exec.compilation_mode, "opt");
        assert_eq!(exec.platform, target.host_platform);
        assert!(exec.output_directory.starts_with(&format!("{}-opt-exec-ST-", host_cpu())));
        assert_eq!(exec.exec(), target.exec());
    }

    #[test]
    fn native_options_are_sorted_and_their_defaults_parse() {
        let names: Vec<&str> = NATIVE_OPTIONS.iter().map(|option| option.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        let options = options();
        for option in NATIVE_OPTIONS {
            assert!(options.get(option.name).is_ok(), "{}", option.name);
        }
    }

    #[test]
    fn options_given_their_defaults_are_unchanged() {
        let mut options = options();
        options.apply("compilation_mode", "fastbuild", &parse_label).unwrap();
        options.apply("stamp", "false", &parse_label).unwrap();
        assert_eq!(options, BuildOptions::default());
        options.apply("compilation_mode", "opt", &parse_label).unwrap();
        assert_eq!(options.changed().len(), 1);
    }

    #[test]
    fn options_parse_by_type() {
        let mut options = options();
        options.apply("copt", "-O2", &parse_label).unwrap();
        options.apply("copt", "-g", &parse_label).unwrap();
        options.apply("define", "b=1", &parse_label).unwrap();
        options.apply("define", "a=2", &parse_label).unwrap();
        options.apply("define", "b=3", &parse_label).unwrap();
        options.apply("platforms", "@@a//:x,@@b//:y", &parse_label).unwrap();
        options.apply("platforms", "@@c//:z", &parse_label).unwrap();
        options.apply("extra_toolchains", "@@a//:t1,@@a//:t2", &parse_label).unwrap();
        options.apply("extra_toolchains", "@@a//:t3", &parse_label).unwrap();
        assert_eq!(options.strings("copt"), ["-O2", "-g"]);
        assert_eq!(options.define("b"), Some("3"));
        assert_eq!(options.get("define").unwrap(), &OptionValue::Assignments(vec![("a".into(), "2".into()), ("b".into(), "3".into())]));
        assert_eq!(options.labels("platforms"), [Label::parse_canonical("@@c//:z").unwrap()]);
        assert_eq!(options.strings("extra_toolchains"), ["@@a//:t1", "@@a//:t2", "@@a//:t3"]);
    }

    #[test]
    fn bad_values_are_errors_as_bazel_words_them() {
        let mut options = options();
        let error = options.apply("compilation_mode", "fast", &parse_label).unwrap_err();
        assert_eq!(error.to_string(), "While parsing option --compilation_mode=fast: Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)");
        let error = options.apply("define", "a", &parse_label).unwrap_err();
        assert_eq!(error.to_string(), "While parsing option --define=a: Variable definitions must be in the form of a 'name=value' assignment");
        let error = options.apply("strict_proto_deps", "bogus", &parse_label).unwrap_err();
        assert_eq!(
            error.to_string(),
            "While parsing option --strict_proto_deps=bogus: Not a valid strict dependency checking level: 'bogus' (should be off, warn, error, strict or default)"
        );
        let error = options.apply("stamp", "maybe", &parse_label).unwrap_err();
        assert_eq!(error.to_string(), "While parsing option --stamp=maybe: 'maybe' is not a boolean");
        options.apply("compilation_mode", "OPT", &parse_label).unwrap();
        assert_eq!(options.string("compilation_mode"), "opt");
        assert!(matches!(options.apply("nosuch", "1", &parse_label), Err(OptionError::Unknown(_))));
    }

    #[test]
    fn config_settings_match_as_bazels_do() {
        let mut options = options();
        options.apply("copt", "-O2", &parse_label).unwrap();
        options.apply("copt", "-g", &parse_label).unwrap();
        options.apply("define", "mode=fast", &parse_label).unwrap();
        assert!(options.matches("copt", "-g", &parse_label).unwrap());
        assert!(!options.matches("copt", "-O3", &parse_label).unwrap());
        assert!(options.matches("define", "mode=fast", &parse_label).unwrap());
        assert!(!options.matches("define", "mode=slow", &parse_label).unwrap());
        assert!(options.matches("stamp", "0", &parse_label).unwrap());
        assert!(options.matches("experimental_platform_in_output_dir", "auto", &parse_label).unwrap());
        assert!(!options.matches("experimental_platform_in_output_dir", "false", &parse_label).unwrap());
        assert!(options.matches("conlyopt", "", &parse_label).is_ok_and(|matched| !matched));
        assert!(!options.matches("compiler", "gcc", &parse_label).unwrap());
        assert!(options.matches("nosuch", "1", &parse_label).is_err());
    }
}
