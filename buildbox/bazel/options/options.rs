//! Bazel's command lines: startup options, the command, its options from
//! `.bazelrc` files and the command line, and the residue (target patterns
//! or a query).

mod rc;

pub use rc::{RcFile, RcError, parse_rc_files};

use std::collections::HashMap;

/// The kind of value an option takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OptionKind {
    /// `--flag`, `--noflag`, or `--flag=true|false|yes|no|1|0`.
    Bool,
    /// `--flag=value` or `--flag value`; the last one given wins.
    Value,
    /// `--flag=value` or `--flag value`, which may be given many times.
    Multi,
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

/// Options after parsing: each option's values in the order given, and the
/// arguments that weren't options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedOptions {
    values: Vec<(String, String)>,
    pub residue: Vec<String>,
    /// Options this command doesn't know, which were ignored.
    pub unknown: Vec<String>,
}

impl ParsedOptions {
    /// The last value given for `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.iter().rev().find(|(option, _)| option == name).map(|(_, value)| value.as_str())
    }

    /// Every value given for `name`, in order.
    pub fn all(&self, name: &str) -> Vec<&str> {
        self.values.iter().filter(|(option, _)| option == name).map(|(_, value)| value.as_str()).collect()
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

    #[error("config value '{0}' is not defined in any .rc file")]
    UnknownConfig(String),

    #[error("config expansion of '{0}' is recursive")]
    RecursiveConfig(String),
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

/// Parse `args` against `spec`. `--config=name` is expanded with `rc`'s
/// definitions for `command`. Unknown options are recorded and skipped, so
/// tools that pass options this implementation lacks still work.
pub fn parse(args: &[String], spec: &OptionSpec, rc: &RcFile, command: &str) -> Result<ParsedOptions, OptionsError> {
    let mut parsed = ParsedOptions::default();
    let mut expanding = Vec::new();
    parse_into(args, spec, rc, command, &mut parsed, &mut expanding)?;
    Ok(parsed)
}

fn parse_into(
    args: &[String],
    spec: &OptionSpec,
    rc: &RcFile,
    command: &str,
    parsed: &mut ParsedOptions,
    expanding: &mut Vec<String>,
) -> Result<(), OptionsError> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;
        if arg == "--" {
            parsed.residue.extend(args[index..].iter().cloned());
            return Ok(());
        }
        let Some(option) = arg.strip_prefix("--") else {
            if arg == "-k" {
                parsed.values.push(("keep_going".into(), "true".into()));
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
            Some((name, _, _)) => {
                let value = match inline {
                    Some(value) => value,
                    None => next_value(args, &mut index, name)?,
                };
                parsed.values.push((name.to_string(), value));
            }
            None => {
                parsed.unknown.push(arg.clone());
                if inline.is_none() && args.get(index).is_some_and(|next| !next.starts_with('-') && looks_like_value(next)) {
                    index += 1;
                }
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

/// Whether an argument after an unknown `--option` is probably its value
/// rather than a target pattern or query.
fn looks_like_value(next: &str) -> bool {
    !next.contains("//") && !next.contains(':') && !next.contains('(')
}

/// Find an option's kind, allowing `--noname` for boolean options.
fn option_kind<'a>(spec: &OptionSpec, name: &'a str) -> Option<(&'a str, OptionKind, bool)> {
    if let Some(kind) = spec.kind(name) {
        return Some((name, kind, false));
    }
    let stripped = name.strip_prefix("no")?;
    match spec.kind(stripped) {
        Some(OptionKind::Bool) => Some((stripped, OptionKind::Bool, true)),
        _ => None,
    }
}

fn next_value(args: &[String], index: &mut usize, name: &str) -> Result<String, OptionsError> {
    let Some(value) = args.get(*index) else {
        return Err(OptionsError::MissingValue(name.to_string()));
    };
    *index += 1;
    Ok(value.clone())
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
    parsed.unknown.extend(expanded.unknown);
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
pub fn command_args(rc: &RcFile, command: &str, command_line: &[String]) -> Vec<String> {
    let mut args = rc.command_args("always");
    args.extend(rc.command_args("common"));
    for inherited in inherited_commands(command) {
        args.extend(rc.command_args(inherited));
    }
    let platform = host_platform_config();
    if platform_specific_config_enabled(args.iter().chain(command_line)) && rc.config_args(command, platform).is_some() {
        args.push(format!("--config={platform}"));
    }
    args.extend(command_line.iter().cloned());
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
