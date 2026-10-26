//! Build configurations: the options a target is built with.
//!
//! A target can be built in several configurations in one build: for the
//! target platform, and for the execution platform when it's a tool another
//! target uses. Each configuration is interned, so configured targets refer
//! to theirs by a 4-byte [`ConfigId`].

mod options;

pub use options::{BuildOptions, FlagValue, LabelParser, NATIVE_OPTIONS, NativeOption, OptionDefault, OptionError, OptionType, OptionValue, command_line_options, native_option};

use bazel_label::Label;
use intern::BlobTable;
use skycodec::{Decode, Encode};
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
    /// For a toolchain target: the execution platform of the rule it's
    /// analyzed for, on which its tools run.
    pub toolchain_exec_platform: Option<Label>,
    /// For a target's key that names aspects applied to it: those aspects,
    /// in the order they apply.
    pub aspects: Vec<AspectDescriptor>,
    /// The name of its output directory under `bazel-out`, such as
    /// `k8-fastbuild` or `k8-opt-exec-ST-d57f47055a04`.
    pub output_directory: String,
    /// A short hash of the options, as `cquery` prints it.
    pub checksum: String,
    /// The hash in full, as a failed command's configuration.
    pub digest: String,
}

impl ConfigId {
    /// Intern a configuration.
    pub fn intern(options: &BuildOptions) -> ConfigId {
        ConfigId(CONFIGS.table.intern(&skycodec::to_bytes(options)))
    }

    /// The configuration with this id.
    pub fn get(self) -> Arc<BuildConfiguration> {
        if let Some(config) = self.decoded() {
            return config;
        }
        let options: BuildOptions = skycodec::from_bytes(CONFIGS.table.bytes(self.0)).expect("interned options decode");
        let config = Arc::new(BuildConfiguration::new(self, options));
        let mut decoded = CONFIGS.decoded.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let index = self.0 as usize;
        if decoded.len() <= index {
            decoded.resize(index + 1, None);
        }
        decoded[index].get_or_insert(config).clone()
    }

    /// The configuration, if it's been decoded already. Decoding happens
    /// outside the lock, since naming a configuration made by a transition
    /// gets its baseline's.
    fn decoded(self) -> Option<Arc<BuildConfiguration>> {
        let decoded = CONFIGS.decoded.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        decoded.get(self.0 as usize).cloned().flatten()
    }

    pub fn id(self) -> u32 {
        self.0
    }
}

/// Every configuration interned, in id order, as its encoded options: what
/// ids written down in this process refer to.
pub fn interned_configs() -> Vec<Vec<u8>> {
    (0..CONFIGS.table.len()).map(|id| CONFIGS.table.bytes(id).to_vec()).collect()
}

/// Intern configurations [`interned_configs`] gave, as those with ids from
/// 0, before any other is interned, so that ids saved by another process
/// mean the same here.
pub fn restore_configs(saved: &[Vec<u8>]) -> Result<(), String> {
    for (expected, options) in (0..).zip(saved) {
        let got = CONFIGS.table.intern(options);
        if got != expected {
            return Err(format!("configuration {expected} was interned as {got}"));
        }
    }
    Ok(())
}

impl fmt::Debug for ConfigId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get().checksum)
    }
}

impl BuildConfiguration {
    fn new(id: ConfigId, options: BuildOptions) -> BuildConfiguration {
        let digest = options_digest(&options);
        let compilation_mode = options.string("compilation_mode").to_string();
        let cpu = options.string("cpu").to_string();
        let host_platform = options.label("host_platform").unwrap_or(*DEFAULT_HOST_PLATFORM);
        let platform = options.labels("platforms").first().copied().unwrap_or(host_platform);
        let is_exec = options.bool("is exec configuration");
        let toolchain_exec_platform = options.label(TOOLCHAIN_EXEC_PLATFORM);
        let aspects = options.strings(APPLIED_ASPECTS).iter().filter_map(|text| AspectDescriptor::parse(text)).collect();
        let named = platform_in_output_directory(&options, platform, is_exec);
        let descriptor = match named {
            true => platform_descriptor(&options, platform, &cpu),
            false => cpu.clone(),
        };
        let mnemonic = match is_exec {
            true => format!("{descriptor}-{compilation_mode}-exec"),
            false => format!("{descriptor}-{compilation_mode}"),
        };
        let in_mnemonic = if named { "platforms" } else { "cpu" };
        let output_directory = match options.baseline {
            None => mnemonic,
            Some(baseline) => transitioned_directory(&mnemonic, &options, &baseline.get().options, in_mnemonic),
        };
        BuildConfiguration {
            id,
            options,
            compilation_mode,
            cpu,
            platform,
            host_platform,
            is_exec,
            toolchain_exec_platform,
            aspects,
            output_directory,
            checksum: digest[..7].to_string(),
            digest,
        }
    }

    /// The directory outputs of `repo`'s packages go in, relative to the
    /// execution root, such as `bazel-out/k8-fastbuild/bin`.
    pub fn bin_dir(&self) -> String {
        format!("bazel-out/{}/bin", self.output_directory)
    }

    /// The configuration a toolchain target is analyzed in for a rule whose
    /// tools run on `platform`: this one, with tools of its own on that
    /// platform too, as Bazel's toolchain transition gives.
    pub fn for_toolchain(&self, platform: Label) -> ConfigId {
        let mut options = self.options.clone();
        options.set(TOOLCHAIN_EXEC_PLATFORM, OptionValue::Label(platform)).expect("a native option");
        ConfigId::intern(&options)
    }

    /// The configuration a target's dependencies are built in: this one,
    /// less the execution platform a toolchain target is analyzed for and
    /// the aspects applied to the target.
    pub fn for_dependencies(&self) -> ConfigId {
        if self.toolchain_exec_platform.is_none() && self.aspects.is_empty() {
            return self.id;
        }
        let mut options = self.options.clone();
        options.set(TOOLCHAIN_EXEC_PLATFORM, OptionValue::Null).expect("a native option");
        options.set(APPLIED_ASPECTS, OptionValue::Strings(Vec::new())).expect("a native option");
        ConfigId::intern(&options)
    }

    /// The configuration naming `aspects` applied, after those this one
    /// names, each once.
    pub fn with_aspects(&self, aspects: &[AspectDescriptor]) -> ConfigId {
        let mut applied = self.aspects.clone();
        for aspect in aspects {
            if !applied.contains(aspect) {
                applied.push(aspect.clone());
            }
        }
        self.applying(&applied)
    }

    /// This configuration, naming no aspects.
    pub fn without_aspects(&self) -> ConfigId {
        match self.aspects.is_empty() {
            true => self.id,
            false => self.applying(&[]),
        }
    }

    fn applying(&self, aspects: &[AspectDescriptor]) -> ConfigId {
        let applied = aspects.iter().map(AspectDescriptor::to_string).collect();
        let mut options = self.options.clone();
        options.set(APPLIED_ASPECTS, OptionValue::Strings(applied)).expect("a native option");
        ConfigId::intern(&options)
    }

    /// A configuration for tools that run on `platform`, built optimised:
    /// what Bazel's exec transition gives where bb has no builtins to run
    /// it.
    pub fn exec(&self, platform: Label) -> ConfigId {
        let mut options = self.options.clone();
        options.baseline = None;
        let exec_options = [
            ("is exec configuration", OptionValue::Bool(true)),
            ("compilation_mode", OptionValue::String("opt".to_string())),
            ("platforms", OptionValue::Labels(vec![platform])),
        ];
        for (name, value) in exec_options {
            options.set(name, value).expect("exec options are native options");
        }
        ConfigId::intern(&options)
    }
}

/// Whether an output directory's name starts with the target platform's
/// rather than the CPU's, as `--experimental_platform_in_output_dir` says:
/// for exec configurations unless it's `no`, and for others if it's `yes`.
fn platform_in_output_directory(options: &BuildOptions, platform: Label, is_exec: bool) -> bool {
    let setting = options.string("experimental_platform_in_output_dir");
    if is_exec {
        return setting != "no";
    }
    let limit = options.labels("incompatible_limit_platforms_in_output_dir_to");
    setting == "yes" && (limit.is_empty() || limit.contains(&platform))
}

/// The platform as an output directory's name gives it: by its name, as
/// Bazel's legacy heuristic does, unless that's one of the default
/// platforms' names, for which the CPU stands.
fn platform_descriptor(options: &BuildOptions, platform: Label, cpu: &str) -> String {
    if options.labels("platforms").len() > 1 {
        return "multi-platform".to_string();
    }
    let name = platform.name.as_str();
    match DEFAULT_PLATFORM_NAMES.contains(&name) {
        true => cpu.to_string(),
        false => name.to_string(),
    }
}

/// The names of Bazel's default platforms, which output directories don't
/// name.
const DEFAULT_PLATFORM_NAMES: [&str; 5] = ["host", "host_platform", "target_platform", "default_host", "default_target"];

/// The output directory of options a transition made: `mnemonic`, then,
/// if they differ from the baseline's in more than the mnemonic says, a
/// hash of how, as Bazel names it. `in_mnemonic` is the option the mnemonic
/// names beside the compilation mode: `cpu` or `platforms`.
fn transitioned_directory(mnemonic: &str, options: &BuildOptions, baseline: &BuildOptions, in_mnemonic: &str) -> String {
    let changed = changed_settings(options, baseline, in_mnemonic);
    if changed.is_empty() {
        return mnemonic.to_string();
    }
    let mut fingerprint = ring::digest::Context::new(&ring::digest::SHA256);
    for setting in &changed {
        fingerprint.update(&varint(setting.len()));
        fingerprint.update(setting.as_bytes());
    }
    let digest = fingerprint.finish();
    let hex: String = digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{mnemonic}-ST-{}", &hex[..12])
}

/// The settings that differ from the baseline's, each as `name=value`, or
/// `name@null` for none, sorted by name. The options the mnemonic names,
/// the compilation mode and `in_mnemonic`, are left out.
fn changed_settings(options: &BuildOptions, baseline: &BuildOptions, in_mnemonic: &str) -> Vec<String> {
    let mut changed = std::collections::BTreeMap::new();
    for option in NATIVE_OPTIONS {
        if option.name == in_mnemonic || matches!(option.name, "compilation_mode" | TOOLCHAIN_EXEC_PLATFORM | APPLIED_ASPECTS) {
            continue;
        }
        let (Ok(value), Ok(base)) = (options.get(option.name), baseline.get(option.name)) else {
            continue;
        };
        if value != base {
            changed.insert(format!("//command_line_option:{}", option.name), option.java_text(value));
        }
    }
    let labels: std::collections::BTreeSet<Label> = options.starlark_flags.iter().chain(&baseline.starlark_flags).map(|(label, _)| *label).collect();
    for label in labels {
        let value = options.flag(label);
        if value != baseline.flag(label) {
            changed.insert(label.to_string(), value.map(FlagValue::java_text));
        }
    }
    changed
        .into_iter()
        .map(|(name, value)| match value {
            Some(value) => format!("{name}={value}"),
            None => format!("{name}@null"),
        })
        .collect()
}

/// `n` as a protobuf varint, as Bazel's fingerprints prefix strings with
/// their lengths.
fn varint(mut n: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let low = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            bytes.push(low);
            return bytes;
        }
        bytes.push(low | 0x80);
    }
}

/// bb's own option for the execution platform a toolchain target is
/// analyzed for.
const TOOLCHAIN_EXEC_PLATFORM: &str = "toolchain execution platform";

/// bb's own option for the aspects a key applies to its target.
const APPLIED_ASPECTS: &str = "applied aspects";

/// An aspect applied to targets: the `.bzl` file that exports it, its
/// name, and the values of its parameters, as Bazel writes them:
/// `//pkg:defs.bzl%name[param="value"]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AspectDescriptor {
    pub file: Label,
    pub name: String,
    /// Each parameter's name and value, as Starlark writes the value,
    /// sorted by name.
    pub params: Vec<(String, String)>,
}

impl AspectDescriptor {
    /// Read a descriptor as `Display` writes it.
    pub fn parse(text: &str) -> Option<AspectDescriptor> {
        let (aspect, params) = match text.split_once('[') {
            Some((aspect, params)) => (aspect, params.strip_suffix(']')?),
            None => (text, ""),
        };
        let (file, name) = aspect.rsplit_once('%')?;
        let mut parsed = Vec::new();
        for param in split_params(params) {
            let (name, value) = param.split_once('=')?;
            parsed.push((name.to_string(), value.to_string()));
        }
        Some(AspectDescriptor {
            file: Label::parse_canonical(file).ok()?,
            name: name.to_string(),
            params: parsed,
        })
    }

    /// The descriptor as Bazel describes it to users and to
    /// `ctx.aspect_ids`: each parameter's value quoted as Java prints it,
    /// so `"a"` and `True` are `"a"` and `"true"`.
    pub fn description(&self) -> String {
        let aspect = format!("{}%{}", self.file, self.name);
        if self.params.is_empty() {
            return aspect;
        }
        let params: Vec<String> = self.params.iter().map(|(name, value)| format!("{name}=\"{}\"", described_value(value))).collect();
        format!("{aspect}[{}]", params.join(","))
    }
}

/// A parameter's value, as Starlark writes it, as Java prints it, with
/// quotes and backslashes escaped.
fn described_value(value: &str) -> String {
    let text = match value {
        "True" => "true".to_string(),
        "False" => "false".to_string(),
        _ => unquoted(value).unwrap_or_else(|| value.to_string()),
    };
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A Starlark string literal's text, as a descriptor's parameter has it.
pub fn unquoted(literal: &str) -> Option<String> {
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::with_capacity(inner.len());
    let mut escaped = false;
    for c in inner.chars() {
        match c {
            '\\' if !escaped => escaped = true,
            _ => {
                escaped = false;
                text.push(c);
            }
        }
    }
    Some(text)
}

impl fmt::Display for AspectDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}%{}", self.file, self.name)?;
        if self.params.is_empty() {
            return Ok(());
        }
        let params: Vec<String> = self.params.iter().map(|(name, value)| format!("{name}={value}")).collect();
        write!(f, "[{}]", params.join(","))
    }
}

/// A SHA-256 hash of the options, as long as Bazel's, but for bb's own,
/// which Bazel keeps in targets' keys: a target's configuration is its
/// toolchains' and aspects' too.
fn options_digest(options: &BuildOptions) -> String {
    let mut options = options.clone();
    options.set(APPLIED_ASPECTS, OptionValue::Strings(Vec::new())).expect("a native option");
    options.set(TOOLCHAIN_EXEC_PLATFORM, OptionValue::Null).expect("a native option");
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("{options:?}").as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parameters as `Display` joins them, split at the commas outside quoted
/// strings.
fn split_params(params: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, c) in params.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&params[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < params.len() {
        parts.push(&params[start..]);
    }
    parts
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
        let exec = target.exec(target.host_platform).get();
        assert!(exec.is_exec);
        assert_eq!(exec.compilation_mode, "opt");
        assert_eq!(exec.platform, target.host_platform);
        assert_eq!(exec.output_directory, format!("{}-opt-exec", host_cpu()));
        assert_eq!(exec.exec(target.host_platform), target.exec(target.host_platform));
    }

    #[test]
    fn transitioned_options_are_named_as_bazel_names_them() {
        let baseline = ConfigId::intern(&options());
        let mut transitioned = options();
        transitioned.baseline = Some(baseline);
        assert_eq!(ConfigId::intern(&transitioned).get().output_directory, format!("{}-fastbuild", host_cpu()));
        transitioned.apply("compilation_mode", "opt", &parse_label).unwrap();
        assert_eq!(ConfigId::intern(&transitioned).get().output_directory, format!("{}-opt", host_cpu()));
        transitioned.apply("compilation_mode", "fastbuild", &parse_label).unwrap();
        transitioned.apply("copt", "-DX", &parse_label).unwrap();
        transitioned.starlark_flags.push((Label::parse_canonical("//:flag").unwrap(), FlagValue::String("b".to_string())));
        assert_eq!(ConfigId::intern(&transitioned).get().output_directory, format!("{}-fastbuild-ST-b4b1d2ee8ba0", host_cpu()));
        let mut listed = options();
        listed.baseline = Some(baseline);
        let label = Label::parse_canonical("//:l").unwrap();
        listed.set_flag(label, FlagValue::Strings(vec!["p".into(), "q".into()]), &FlagValue::Strings(vec!["a".into()]));
        listed.set_flag(Label::parse_canonical("//:b").unwrap(), FlagValue::Bool(true), &FlagValue::Bool(false));
        assert_eq!(ConfigId::intern(&listed).get().output_directory, format!("{}-fastbuild-ST-045eccb39967", host_cpu()));
    }

    #[test]
    fn aspect_descriptors_read_back_as_they_are_written() {
        let text = r#"//:defs.bzl%count[mode="a,b",n=3]"#;
        let descriptor = AspectDescriptor::parse(text).unwrap();
        assert_eq!(descriptor.params, [("mode".to_string(), r#""a,b""#.to_string()), ("n".to_string(), "3".to_string())]);
        assert_eq!(descriptor.to_string(), text);
        let applied = ConfigId::intern(&options()).get().with_aspects(&[descriptor.clone()]).get();
        assert_eq!(applied.aspects, [descriptor]);
        assert_eq!(applied.output_directory, format!("{}-fastbuild", host_cpu()));
        assert_eq!(applied.for_dependencies(), ConfigId::intern(&options()));
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

    #[test]
    fn aspects_are_described_as_bazel_describes_them() {
        let aspect = AspectDescriptor {
            file: Label::parse_canonical("//pkg:defs.bzl").unwrap(),
            name: "count".to_string(),
            params: vec![("flag".to_string(), "True".to_string()), ("mode".to_string(), r#""a \"b\"""#.to_string()), ("n".to_string(), "3".to_string())],
        };
        assert_eq!(aspect.description(), r#"//pkg:defs.bzl%count[flag="true",mode="a \"b\"",n="3"]"#);
        let plain = AspectDescriptor { params: Vec::new(), ..aspect };
        assert_eq!(plain.description(), "//pkg:defs.bzl%count");
    }
}
