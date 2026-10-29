//! Bazel's command lines: startup options, the command, its options from
//! `.bazelrc` files and the command line, and the residue (target patterns
//! or a query).

mod bazel;
mod duration;
mod jobs;
mod rc;

pub use bazel::{BazelKind, BazelOption, bazel_option};
pub use duration::convert_duration;
pub use jobs::{MAX_JOBS, convert_jobs};
pub use rc::{RcFile, RcError, parse_rc_files};

use std::collections::HashMap;

/// The kind of value an option takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OptionKind {
    /// `--flag`, `--noflag`, or `--flag=true|false|yes|no|1|0`.
    Bool,
    /// A [`OptionKind::Bool`] that may also be `auto`, recorded as `true`,
    /// `false` or `auto`.
    TriState,
    /// `--flag=value` or `--flag value`; the last one given wins.
    Value,
    /// A [`OptionKind::Value`] that must be an integer.
    Int,
    /// `--flag=value` or `--flag value`, which may be given many times.
    Multi,
    /// A count of jobs, as `--jobs` takes it: an integer or a keyword such
    /// as `HOST_CPUS*.5`. It's recorded as the count it comes to.
    Jobs,
}

/// The options a command knows, and their kinds.
#[derive(Clone, Debug, Default)]
pub struct OptionSpec {
    kinds: HashMap<&'static str, OptionKind>,
}

impl OptionSpec {
    pub fn new(options: &[(&'static str, OptionKind)]) -> OptionSpec {
        OptionSpec {
            kinds: options.iter().copied().collect(),
        }
    }

    pub fn kind(&self, name: &str) -> Option<OptionKind> {
        self.kinds.get(name).copied()
    }

    /// Every option's name and kind, sorted by name.
    pub fn options(&self) -> Vec<(&'static str, OptionKind)> {
        let mut options: Vec<(&'static str, OptionKind)> = self.kinds.iter().map(|(name, kind)| (*name, *kind)).collect();
        options.sort();
        options
    }
}

/// An argument to parse, and whether a `common` line of a `.bazelrc` file
/// gave it: such options apply to the commands that take them, and other
/// commands ignore them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arg {
    pub text: String,
    pub common: bool,
}

impl Arg {
    /// An argument given for the command itself.
    pub fn given(text: &str) -> Arg {
        Arg { text: text.to_string(), common: false }
    }

    /// An argument from a `common` line of a `.bazelrc` file.
    pub fn common(text: &str) -> Arg {
        Arg { text: text.to_string(), common: true }
    }
}

/// Arguments given for the command itself.
pub fn given_args(args: &[String]) -> Vec<Arg> {
    args.iter().map(|arg| Arg::given(arg)).collect()
}

/// Options after parsing: each option's values in the order given, and the
/// arguments that weren't options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedOptions {
    values: Vec<(String, String)>,
    pub residue: Vec<String>,
    /// Each use of an option bb doesn't implement, which is an error unless
    /// it leaves the option as it is by default.
    unsupported: Vec<UnsupportedUse>,
}

/// A use of an option bb doesn't implement.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnsupportedUse {
    option: &'static BazelOption,
    spelled: String,
    is_default: bool,
}

impl ParsedOptions {
    /// Set `name` to `value`, as if the command line ended with it.
    pub fn set(&mut self, name: &str, value: &str) {
        self.values.push((name.to_string(), value.to_string()));
    }

    /// The last value given for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(option, _)| option == name).map(|(_, value)| value.as_str())
    }

    /// Every value given for `name`, in order.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.values.iter().filter(|(option, _)| option == name).map(|(_, value)| value.as_str()).collect()
    }

    /// An integer option's value, or `default` if it wasn't given.
    pub fn int(&self, name: &str, default: i64) -> i64 {
        match self.get(name) {
            Some(value) => value.parse().unwrap_or(default),
            None => default,
        }
    }

    /// A boolean option's value, or `default` if it wasn't given.
    pub fn flag(&self, name: &str, default: bool) -> bool {
        match self.get(name) {
            Some(value) => parse_bool(value).unwrap_or(default),
            None => default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionsError {
    #[error("expected a value after '--{0}'")]
    MissingValue(String),

    #[error("invalid value '{value}' for boolean option '--{name}'")]
    InvalidBool { name: String, value: String },

    #[error("While parsing option --{name}={value}: '{value}' is not an int")]
    InvalidInt { name: String, value: String },

    #[error("While parsing option {spelled}: '{value}' is not a boolean or 'auto'")]
    InvalidTriState { spelled: String, value: String },

    #[error("While parsing option {spelled}: {message}")]
    InvalidValue { spelled: String, message: String },

    #[error("config value '{0}' is not defined in any .rc file")]
    UnknownConfig(String),

    #[error("config expansion of '{0}' is recursive")]
    RecursiveConfig(String),

    #[error("{0} :: Unrecognized option: {0}")]
    Unrecognized(String),

    #[error("{spelled} :: bb doesn't support --{name} yet")]
    Unsupported { spelled: String, name: String },
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Parse `args` against `spec`, the options bb implements for `command`.
/// `--config=name` is expanded with `rc`'s definitions for `command`.
///
/// Options Bazel doesn't take for `command` are errors, unless a `common`
/// line gave them. Options Bazel takes that `spec` lacks are errors unless
/// they're given their default values, so that nothing bb can't do is
/// silently skipped.
pub fn parse(args: &[Arg], spec: &OptionSpec, rc: &RcFile, command: &str) -> Result<ParsedOptions, OptionsError> {
    let mut parsed = ParsedOptions::default();
    let mut expanding = Vec::new();
    parse_into(args, spec, rc, command, &mut parsed, &mut expanding)?;
    if let Some(error) = unsupported_error(&parsed.unsupported) {
        return Err(error);
    }
    Ok(parsed)
}

/// The error for the first use of an option bb doesn't implement that
/// changes it: for an option given many times, any use with a value; for
/// others, the last use, which is the one that counts.
fn unsupported_error(uses: &[UnsupportedUse]) -> Option<OptionsError> {
    let changing = uses.iter().find(|used| {
        let last = uses.iter().rev().find(|other| other.option.name == used.option.name);
        let counts = used.option.kind == BazelKind::Multi || last.is_some_and(|last| std::ptr::eq(last, *used));
        counts && !used.is_default
    })?;
    Some(OptionsError::Unsupported {
        spelled: changing.spelled.clone(),
        name: changing.option.name.to_string(),
    })
}

fn parse_into(
    args: &[Arg],
    spec: &OptionSpec,
    rc: &RcFile,
    command: &str,
    parsed: &mut ParsedOptions,
    expanding: &mut Vec<String>,
) -> Result<(), OptionsError> {
    let mut index = 0;
    while index < args.len() {
        let Arg { text: arg, common } = &args[index];
        index += 1;
        if arg == "--" {
            parsed.residue.extend(args[index..].iter().map(|arg| arg.text.clone()));
            return Ok(());
        }
        let Some(option) = arg.strip_prefix("--") else {
            if arg == "-k" {
                parsed.values.push(("keep_going".into(), "true".into()));
                continue;
            }
            if arg == "-j" && spec.kind("jobs") == Some(OptionKind::Jobs) {
                let value = next_value(args, &mut index, "jobs")?;
                let jobs = jobs_value(format!("-j {value}"), &value)?;
                parsed.values.push(("jobs".into(), jobs));
                continue;
            }
            if arg == "-c" {
                let value = next_value(args, &mut index, "compilation_mode")?;
                parsed.values.push(("compilation_mode".into(), value));
                continue;
            }
            parsed.residue.push(arg.clone());
            continue;
        };
        if let Some(flag) = starlark_flag(option) {
            parsed.values.push((STARLARK_FLAG.to_string(), flag));
            continue;
        }
        let (name, inline) = match option.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (option, None),
        };
        if name == "config" {
            let config = match inline {
                Some(value) => value,
                None => next_value(args, &mut index, name)?,
            };
            expand_config(&config, spec, rc, command, parsed, expanding)?;
            continue;
        }
        if !bazel_takes(name, command) {
            if !common || bazel_option_named(name).is_none() {
                return Err(OptionsError::Unrecognized(arg.clone()));
            }
            skip_value(name, inline.is_some(), &mut index);
            continue;
        }
        match option_kind(spec, name) {
            Some((name, OptionKind::Bool, negated)) => {
                let value = match inline {
                    Some(value) => parse_bool(&value).ok_or(OptionsError::InvalidBool {
                        name: name.to_string(),
                        value: value.clone(),
                    })?,
                    None => true,
                };
                parsed.values.push((name.to_string(), (value != negated).to_string()));
            }
            Some((name, OptionKind::TriState, negated)) => {
                let value = match inline.as_deref() {
                    Some("auto") if !negated => "auto".to_string(),
                    Some(value) => match parse_bool(value) {
                        Some(value) => (value != negated).to_string(),
                        None => return Err(OptionsError::InvalidTriState { spelled: arg.clone(), value: value.to_string() }),
                    },
                    None => (!negated).to_string(),
                };
                parsed.values.push((name.to_string(), value));
            }
            Some((name, kind, _)) => {
                let (value, spelled) = match inline {
                    Some(value) => (value, arg.clone()),
                    None => {
                        let value = next_value(args, &mut index, name)?;
                        let spelled = format!("{arg} {value}");
                        (value, spelled)
                    }
                };
                if kind == OptionKind::Jobs {
                    parsed.values.push((name.to_string(), jobs_value(spelled, &value)?));
                    continue;
                }
                if kind == OptionKind::Int && value.parse::<i64>().is_err() {
                    return Err(OptionsError::InvalidInt {
                        name: name.to_string(),
                        value,
                    });
                }
                parsed.values.push((name.to_string(), value));
            }
            None => {
                let used = unsupported_use(arg, name, inline, args, &mut index)?;
                parsed.unsupported.push(used);
            }
        }
    }
    Ok(())
}

/// The name under which Starlark flags, such as `--//pkg:flag=value`, are
/// recorded, with values `label=value`.
pub const STARLARK_FLAG: &str = "starlark_flag";

/// A Starlark flag, as `label=value`: `--//pkg:flag=value`,
/// `--@repo//pkg:flag`, or `--no//pkg:flag` for false.
fn starlark_flag(option: &str) -> Option<String> {
    let is_label = |text: &str| text.starts_with("//") || text.starts_with('@');
    if is_label(option) {
        return Some(match option.split_once('=') {
            Some((label, value)) => format!("{label}={value}"),
            None => format!("{option}=true"),
        });
    }
    let negated = option.strip_prefix("no").filter(|rest| is_label(rest))?;
    Some(format!("{negated}=false"))
}

/// Bazel's option `name` names, as `--name` or `--noname`, and whether
/// it's negated.
fn bazel_option_named(name: &str) -> Option<(&'static BazelOption, bool)> {
    if let Some(option) = bazel_option(name) {
        return Some((option, false));
    }
    let option = bazel_option(name.strip_prefix("no")?)?;
    match option.kind {
        BazelKind::Bool | BazelKind::TriState => Some((option, true)),
        _ => None,
    }
}

/// Whether Bazel takes the option `name` names for `command`.
fn bazel_takes(name: &str, command: &str) -> bool {
    bazel_option_named(name).is_some_and(|(option, _)| option.takes(command))
}

/// Step past the value of the option `name` names, if it takes one and it
/// wasn't given inline.
fn skip_value(name: &str, inline: bool, index: &mut usize) {
    let takes_value = bazel_option_named(name).is_some_and(|(option, _)| matches!(option.kind, BazelKind::Value | BazelKind::Multi));
    if takes_value && !inline {
        *index += 1;
    }
}

/// A use of an option bb doesn't implement, which is only accepted if it
/// leaves the option at its default, where Bazel behaves as bb does.
fn unsupported_use(arg: &str, name: &str, inline: Option<String>, args: &[Arg], index: &mut usize) -> Result<UnsupportedUse, OptionsError> {
    let Some((option, negated)) = bazel_option_named(name) else {
        return Err(OptionsError::Unrecognized(arg.to_string()));
    };
    let (value, spelled) = match (option.kind, inline) {
        (BazelKind::Value | BazelKind::Multi, Some(value)) => (Some(value), arg.to_string()),
        (BazelKind::Value | BazelKind::Multi, None) => {
            let value = next_value(args, index, name)?;
            let spelled = format!("{arg} {value}");
            (Some(value), spelled)
        }
        (BazelKind::Bool | BazelKind::TriState, inline) => (Some(flag_value(inline, negated)), arg.to_string()),
        (BazelKind::Expansion, _) => (None, arg.to_string()),
    };
    let is_default = value.is_some_and(|value| option.is_default(&value));
    Ok(UnsupportedUse { option, spelled, is_default })
}

/// A boolean or tristate option's value, as `true`, `false` or as given.
fn flag_value(inline: Option<String>, negated: bool) -> String {
    let value = match inline {
        Some(value) => value,
        None => "true".to_string(),
    };
    match parse_bool(&value) {
        Some(value) => (value != negated).to_string(),
        None => value,
    }
}

/// Find an option's kind, allowing `--noname` for boolean options.
fn option_kind<'a>(spec: &OptionSpec, name: &'a str) -> Option<(&'a str, OptionKind, bool)> {
    if let Some(kind) = spec.kind(name) {
        return Some((name, kind, false));
    }
    let stripped = name.strip_prefix("no")?;
    match spec.kind(stripped) {
        Some(OptionKind::Bool) => Some((stripped, OptionKind::Bool, true)),
        Some(OptionKind::TriState) => Some((stripped, OptionKind::TriState, true)),
        _ => None,
    }
}

/// The count of jobs `value` comes to, as text, or the error naming the
/// option as it was `spelled`.
fn jobs_value(spelled: String, value: &str) -> Result<String, OptionsError> {
    match convert_jobs(value) {
        Ok(jobs) => Ok(jobs.to_string()),
        Err(message) => Err(OptionsError::InvalidValue { spelled, message }),
    }
}

fn next_value(args: &[Arg], index: &mut usize, name: &str) -> Result<String, OptionsError> {
    let Some(value) = args.get(*index) else {
        return Err(OptionsError::MissingValue(name.to_string()));
    };
    *index += 1;
    Ok(value.text.clone())
}

fn expand_config(
    config: &str,
    spec: &OptionSpec,
    rc: &RcFile,
    command: &str,
    parsed: &mut ParsedOptions,
    expanding: &mut Vec<String>,
) -> Result<(), OptionsError> {
    if expanding.iter().any(|name| name == config) {
        return Err(OptionsError::RecursiveConfig(config.to_string()));
    }
    let Some(args) = rc.config_args(command, config) else {
        return Err(OptionsError::UnknownConfig(config.to_string()));
    };
    expanding.push(config.to_string());
    let mut expanded = ParsedOptions::default();
    parse_into(&args, spec, rc, command, &mut expanded, expanding)?;
    expanding.pop();
    parsed.values.extend(expanded.values);
    parsed.unsupported.extend(expanded.unsupported);
    parsed.residue.extend(expanded.residue);
    Ok(())
}

/// The commands each command inherits `.bazelrc` options from, most general
/// first, as Bazel defines them.
pub fn inherited_commands(command: &str) -> Vec<&'static str> {
    let chain: &[&str] = match command {
        "build" => &["build"],
        "test" => &["build", "test"],
        "run" => &["build", "run"],
        "coverage" => &["build", "test", "coverage"],
        "cquery" => &["build", "cquery"],
        "aquery" => &["build", "aquery"],
        "query" => &["query"],
        "fetch" => &["fetch"],
        "sync" => &["fetch", "sync"],
        "info" => &["build", "info"],
        "clean" => &["build", "clean"],
        "mod" => &["mod"],
        "print_action" => &["build", "print_action"],
        "version" => &["version"],
        _ => &[],
    };
    chain.to_vec()
}

/// A command's full argument list: options from `.bazelrc` files for the
/// commands it inherits from, the host platform's config if
/// `--enable_platform_specific_config` asks for it, then the command
/// line's.
pub fn command_args(rc: &RcFile, command: &str, command_line: &[String]) -> Vec<Arg> {
    let mut args = given_args(&rc.command_args("always"));
    args.extend(rc.command_args("common").iter().map(|arg| Arg::common(arg)));
    for inherited in inherited_commands(command) {
        args.extend(given_args(&rc.command_args(inherited)));
    }
    let platform = host_platform_config();
    let texts = args.iter().map(|arg| &arg.text).chain(command_line);
    if platform_specific_config_enabled(texts) && rc.config_args(command, platform).is_some() {
        args.push(Arg::given(&format!("--config={platform}")));
    }
    args.extend(given_args(command_line));
    args
}

/// The name of the host platform's config, as Bazel names it.
fn host_platform_config() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "windows" => "windows",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        _ => "linux",
    }
}

/// Whether the last `--[no]enable_platform_specific_config` enables it.
fn platform_specific_config_enabled<'a>(args: impl Iterator<Item = &'a String>) -> bool {
    let mut enabled = false;
    for arg in args {
        match arg.as_str() {
            "--enable_platform_specific_config" | "--enable_platform_specific_config=true" | "--enable_platform_specific_config=1" => enabled = true,
            "--noenable_platform_specific_config" | "--enable_platform_specific_config=false" | "--enable_platform_specific_config=0" => enabled = false,
            _ => {}
        }
    }
    enabled
}

#[cfg(test)]
mod tests;
