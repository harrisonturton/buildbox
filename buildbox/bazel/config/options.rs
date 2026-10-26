//! Native options: those Bazel's configuration fragments declare, which
//! make up a configuration along with Starlark flags.
//!
//! Each native option bb knows is in [`NATIVE_OPTIONS`], with its type and
//! default. [`BuildOptions`] holds only the values that differ from the
//! defaults, so adding an option doesn't change the configurations that
//! don't set it.

use bazel_label::Label;
use skycodec::{Decode, Encode};
use std::sync::LazyLock;

/// The kind of value a native option has, and how the command line gives
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionType {
    Bool,
    Int,
    String,
    /// `auto`, `yes` or `no`, which `true`, `1`, `false` and `0` also give.
    TriState,
    /// A label, or none.
    Label,
    /// A list of strings: one for each use of the option.
    Strings,
    /// A list of strings, split at commas.
    CommaStrings,
    /// A list of labels, split at commas.
    Labels,
    /// `name=value` pairs, such as `--define`'s.
    Assignments,
}

/// What an option is when the command line doesn't give it.
#[derive(Clone, Copy, Debug)]
pub enum OptionDefault {
    /// The value the command line text gives, such as `fastbuild`. For
    /// lists, an empty text is the empty list.
    Text(&'static str),
    /// No value, as Bazel's `null` defaults: neither an empty string nor an
    /// empty list.
    Null,
    /// This machine's CPU, as Bazel's `--cpu` and `--host_cpu` default to.
    HostCpu,
}

/// A native option.
#[derive(Debug)]
pub struct NativeOption {
    pub name: &'static str,
    pub ty: OptionType,
    pub default: OptionDefault,
    /// For lists: whether each use of the option adds to the list, rather
    /// than replacing it.
    pub multiple: bool,
    /// For strings: the values the option allows, if it restricts them, in
    /// the order Bazel lists them, and what Bazel calls them.
    pub allowed: &'static [&'static str],
    pub allowed_name: &'static str,
}

/// The value of a native option.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum OptionValue {
    /// The value of an option whose default is [`OptionDefault::Null`].
    Null,
    Bool(bool),
    Int(i64),
    String(String),
    Label(Label),
    Strings(Vec<String>),
    Labels(Vec<Label>),
    Assignments(Vec<(String, String)>),
}

/// An option that doesn't exist, or a value it can't have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionError {
    #[error("unknown option: '{0}'")]
    Unknown(String),

    #[error("{0}")]
    Invalid(String),
}

/// Parses a label given on the command line or in a `config_setting`,
/// relative to the main repository.
pub type LabelParser<'a> = &'a dyn Fn(&str) -> Result<Label, String>;

const fn option(name: &'static str, ty: OptionType, default: OptionDefault) -> NativeOption {
    NativeOption {
        name,
        ty,
        default,
        multiple: false,
        allowed: &[],
        allowed_name: "",
    }
}

const fn multiple(name: &'static str, ty: OptionType) -> NativeOption {
    NativeOption {
        name,
        ty,
        default: OptionDefault::Null,
        multiple: true,
        allowed: &[],
        allowed_name: "",
    }
}

const fn choice(name: &'static str, default: &'static str, allowed_name: &'static str, allowed: &'static [&'static str]) -> NativeOption {
    NativeOption {
        name,
        ty: OptionType::String,
        default: OptionDefault::Text(default),
        multiple: false,
        allowed,
        allowed_name,
    }
}

const STRICT_DEPS: &str = "strict dependency checking level";
const STRICT_DEPS_MODES: &[&str] = &["off", "warn", "error", "strict", "default"];

/// The native options bb knows, sorted by name, with Bazel's defaults.
pub static NATIVE_OPTIONS: &[NativeOption] = {
    use OptionDefault::*;
    use OptionType::*;
    &[
        option("apple_platform_type", String, Text("macos")),
        option("collect_code_coverage", Bool, Text("false")),
        choice("compilation_mode", "fastbuild", "compilation mode", &["fastbuild", "dbg", "opt"]),
        option("compiler", String, Null),
        multiple("conlyopt", Strings),
        multiple("copt", Strings),
        option("cpu", String, HostCpu),
        option("crosstool_top", Label, Null),
        multiple("cxxopt", Strings),
        multiple("define", Assignments),
        choice("dynamic_mode", "default", "dynamic mode", &["off", "default", "fully"]),
        option("enable_runfiles", TriState, Text("auto")),
        option("experimental_platform_in_output_dir", TriState, Text("auto")),
        option("experimental_proto_descriptor_sets_include_source_info", Bool, Text("false")),
        option("extra_execution_platforms", CommaStrings, Text("")),
        multiple("extra_toolchains", CommaStrings),
        multiple("features", Strings),
        option("fission", String, Text("no")),
        option("force_pic", Bool, Text("false")),
        multiple("host_copt", Strings),
        option("host_cpu", String, HostCpu),
        option("host_platform", Label, Text("@@platforms//host:host")),
        option("incompatible_enable_cc_toolchain_resolution", Bool, Text("true")),
        multiple("ios_multi_cpus", CommaStrings),
        option("is exec configuration", Bool, Text("false")),
        option("java_language_version", String, Text("")),
        option("java_runtime_version", String, Text("local_jdk")),
        multiple("linkopt", Strings),
        multiple("macos_cpus", CommaStrings),
        option("platforms", Labels, Text("")),
        option("proto_compiler", Label, Text("@@bazel_tools//tools/proto:protoc")),
        multiple("protocopt", Strings),
        option("stamp", Bool, Text("false")),
        choice("strict_proto_deps", "error", STRICT_DEPS, STRICT_DEPS_MODES),
        choice("strict_public_imports", "off", STRICT_DEPS, STRICT_DEPS_MODES),
        choice("strip", "sometimes", "strip mode", &["always", "sometimes", "never"]),
    ]
};

/// The native options the command line can give: all but Bazel's internal
/// ones, whose names have spaces.
pub fn command_line_options() -> impl Iterator<Item = &'static NativeOption> {
    NATIVE_OPTIONS.iter().filter(|option| !option.name.contains(' '))
}

/// The default of every native option, in [`NATIVE_OPTIONS`]' order.
static DEFAULTS: LazyLock<Vec<OptionValue>> = LazyLock::new(|| NATIVE_OPTIONS.iter().map(default_value).collect());

/// The native option named `name`.
pub fn native_option(name: &str) -> Option<&'static NativeOption> {
    let index = NATIVE_OPTIONS.binary_search_by(|option| option.name.cmp(name)).ok()?;
    Some(&NATIVE_OPTIONS[index])
}

/// An option's default value.
pub fn default_of(option: &NativeOption) -> &'static OptionValue {
    let index = NATIVE_OPTIONS.binary_search_by(|candidate| candidate.name.cmp(option.name)).expect("a native option");
    &DEFAULTS[index]
}

fn default_value(option: &NativeOption) -> OptionValue {
    match option.default {
        OptionDefault::Null => null_value(option.ty),
        OptionDefault::HostCpu => OptionValue::String(crate::host_cpu().to_string()),
        OptionDefault::Text(text) => {
            let canonical = |text: &str| Label::parse_canonical(text).map_err(|error| error.to_string());
            option.parse(text, &canonical).expect("native options' defaults parse")
        }
    }
}

/// What an option with a `null` default holds: nothing, or for lists, the
/// empty list, since Bazel's lists start empty.
fn null_value(ty: OptionType) -> OptionValue {
    match ty {
        OptionType::Strings | OptionType::CommaStrings => OptionValue::Strings(Vec::new()),
        OptionType::Labels => OptionValue::Labels(Vec::new()),
        OptionType::Assignments => OptionValue::Assignments(Vec::new()),
        _ => OptionValue::Null,
    }
}

impl NativeOption {
    /// Whether the option's value is a list.
    pub fn is_list(&self) -> bool {
        matches!(self.ty, OptionType::Strings | OptionType::CommaStrings | OptionType::Labels | OptionType::Assignments)
    }

    /// The value one use of the option, `--name=text`, gives.
    pub fn parse(&self, text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, OptionError> {
        self.convert(text, parse_label)
            .map_err(|problem| OptionError::Invalid(format!("While parsing option --{}={text}: {problem}", self.name)))
    }

    fn convert(&self, text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, String> {
        let not_a = |kind: &str| format!("'{text}' is not {kind}");
        match self.ty {
            OptionType::Bool => parse_bool(text).map(OptionValue::Bool).ok_or_else(|| not_a("a boolean")),
            OptionType::Int => text.parse().map(OptionValue::Int).map_err(|_| not_a("an int")),
            OptionType::String => self.convert_string(text),
            OptionType::TriState => parse_tri_state(text).map(|value| OptionValue::String(value.to_string())).ok_or_else(|| format!("Not a valid tri-state (auto, yes, no) option value: '{text}' (should be auto or a boolean)")),
            OptionType::Label if text.is_empty() => Ok(OptionValue::Null),
            OptionType::Label => parse_label(text).map(OptionValue::Label),
            OptionType::Strings => Ok(OptionValue::Strings(vec![text.to_string()])),
            OptionType::CommaStrings => Ok(OptionValue::Strings(comma_separated(text).map(str::to_string).collect())),
            OptionType::Labels => parse_labels(text, parse_label),
            OptionType::Assignments => parse_assignment(text).map(|pair| OptionValue::Assignments(vec![pair])),
        }
    }

    /// A string, which must be one of those allowed, if any are; they're
    /// matched ignoring case, as Bazel's enums are.
    fn convert_string(&self, text: &str) -> Result<OptionValue, String> {
        if self.allowed.is_empty() {
            return Ok(OptionValue::String(text.to_string()));
        }
        match self.allowed.iter().find(|allowed| allowed.eq_ignore_ascii_case(text)) {
            Some(allowed) => Ok(OptionValue::String(allowed.to_string())),
            None => Err(format!("Not a valid {}: '{text}' (should be {})", self.allowed_name, english_list(self.allowed))),
        }
    }

    /// The option's value once `next` is given after `previous`: the two
    /// joined if the option takes many values, or else `next`.
    fn combine(&self, previous: &OptionValue, next: OptionValue) -> OptionValue {
        if !self.multiple {
            return next;
        }
        match (previous, next) {
            (OptionValue::Strings(previous), OptionValue::Strings(next)) => OptionValue::Strings([previous.clone(), next].concat()),
            (OptionValue::Labels(previous), OptionValue::Labels(next)) => OptionValue::Labels([previous.clone(), next].concat()),
            (OptionValue::Assignments(previous), OptionValue::Assignments(next)) => OptionValue::Assignments(normalized(previous, next)),
            (_, next) => next,
        }
    }
}

/// `name=value` pairs with `next` after `previous`, as Bazel normalizes
/// them: the last value of each name, sorted by name.
fn normalized(previous: &[(String, String)], next: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut pairs = previous.to_vec();
    for (name, value) in next {
        pairs.retain(|(existing, _)| *existing != name);
        pairs.push((name, value));
    }
    pairs.sort();
    pairs
}

/// A boolean as Bazel's converter reads one, ignoring case.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Some(true),
        "false" | "0" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

fn parse_tri_state(text: &str) -> Option<&'static str> {
    if text.eq_ignore_ascii_case("auto") {
        return Some("auto");
    }
    parse_bool(text).map(|value| if value { "yes" } else { "no" })
}

fn comma_separated(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').filter(|part| !part.is_empty())
}

fn parse_labels(text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, String> {
    let mut labels = Vec::new();
    for part in comma_separated(text) {
        labels.push(parse_label(part)?);
    }
    Ok(OptionValue::Labels(labels))
}

fn parse_assignment(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once('=') else {
        return Err("Variable definitions must be in the form of a 'name=value' assignment".to_string());
    };
    Ok((name.to_string(), value.to_string()))
}

/// Words joined as Bazel lists them: `a, b or c`.
fn english_list(words: &[&str]) -> String {
    match words.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The options that decide how targets are built.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BuildOptions {
    /// The native options that differ from their defaults, sorted by name.
    native: Vec<(String, OptionValue)>,
    /// Starlark flags, `--//pkg:flag=value`, sorted by label.
    pub starlark_flags: Vec<(Label, String)>,
    /// The environment of actions that use the default shell environment,
    /// resolved against the client's, sorted by name.
    pub action_env: Vec<(String, String)>,
}

impl BuildOptions {
    /// A native option's value.
    pub fn get(&self, name: &str) -> Result<&OptionValue, OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let set = self.native.binary_search_by(|(candidate, _)| candidate.as_str().cmp(name)).ok();
        Ok(match set {
            Some(index) => &self.native[index].1,
            None => default_of(option),
        })
    }

    /// Set a native option, which must be one bb knows, to a value of its
    /// type.
    pub fn set(&mut self, name: &str, value: OptionValue) -> Result<(), OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let index = self.native.binary_search_by(|(candidate, _)| candidate.as_str().cmp(name));
        match (index, value == *default_of(option)) {
            (Ok(index), true) => {
                self.native.remove(index);
            }
            (Ok(index), false) => self.native[index].1 = value,
            (Err(_), true) => {}
            (Err(index), false) => self.native.insert(index, (name.to_string(), value)),
        }
        Ok(())
    }

    /// Give a native option on the command line, `--name=text`, after the
    /// uses of it before.
    pub fn apply(&mut self, name: &str, text: &str, parse_label: LabelParser<'_>) -> Result<(), OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let next = option.parse(text, parse_label)?;
        let value = option.combine(self.get(name)?, next);
        self.set(name, value)
    }

    /// Whether a native option has the value `expected`, as
    /// `config_setting(values = {name: expected})` asks: equal to it, or
    /// for lists, containing all of it. For `name=value` pairs, the last
    /// with its name must have its value.
    pub fn matches(&self, name: &str, expected: &str, parse_label: LabelParser<'_>) -> Result<bool, OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let expected = option.parse(expected, parse_label)?;
        let actual = self.get(name)?;
        Ok(match (actual, &expected) {
            (OptionValue::Strings(actual), OptionValue::Strings(expected)) => list_contains(actual, expected),
            (OptionValue::Labels(actual), OptionValue::Labels(expected)) => list_contains(actual, expected),
            (OptionValue::Assignments(actual), OptionValue::Assignments(expected)) => assignment_matches(actual, expected),
            (actual, expected) => actual == expected,
        })
    }

    /// A string option's value, or `""` if it has none.
    pub fn string(&self, name: &str) -> &str {
        match self.get(name) {
            Ok(OptionValue::String(value)) => value,
            _ => "",
        }
    }

    /// A boolean option's value.
    pub fn bool(&self, name: &str) -> bool {
        matches!(self.get(name), Ok(OptionValue::Bool(true)))
    }

    /// A list of strings' values.
    pub fn strings(&self, name: &str) -> &[String] {
        match self.get(name) {
            Ok(OptionValue::Strings(values)) => values,
            _ => &[],
        }
    }

    /// A label option's value.
    pub fn label(&self, name: &str) -> Option<Label> {
        match self.get(name) {
            Ok(OptionValue::Label(label)) => Some(*label),
            _ => None,
        }
    }

    /// A list of labels' values.
    pub fn labels(&self, name: &str) -> &[Label] {
        match self.get(name) {
            Ok(OptionValue::Labels(labels)) => labels,
            _ => &[],
        }
    }

    /// The value of the last `--define` of `name`.
    pub fn define(&self, name: &str) -> Option<&str> {
        let Ok(OptionValue::Assignments(defines)) = self.get("define") else {
            return None;
        };
        defines.iter().rev().find(|(define, _)| define == name).map(|(_, value)| value.as_str())
    }

    /// The native options that differ from their defaults.
    pub fn changed(&self) -> &[(String, OptionValue)] {
        &self.native
    }
}

fn list_contains<T: PartialEq>(actual: &[T], expected: &[T]) -> bool {
    if actual.is_empty() || expected.is_empty() {
        return actual.is_empty() && expected.is_empty();
    }
    expected.iter().all(|value| actual.contains(value))
}

fn assignment_matches(actual: &[(String, String)], expected: &[(String, String)]) -> bool {
    let [(name, value)] = expected else {
        return false;
    };
    actual.iter().rev().find(|(candidate, _)| candidate == name).is_some_and(|(_, actual)| actual == value)
}
