//! The options Bazel itself takes, and which of its commands take each,
//! from a table generated from Bazel's source.

use std::collections::HashMap;
use std::sync::LazyLock;

/// How Bazel parses an option's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BazelKind {
    /// `--flag`, `--noflag` or `--flag=value`.
    Bool,
    /// A [`BazelKind::Bool`] that may also be `auto`.
    TriState,
    /// An option that stands for others, and takes no value.
    Expansion,
    /// `--flag=value` or `--flag value`.
    Value,
    /// A [`BazelKind::Value`] that may be given many times.
    Multi,
}

/// One of Bazel's options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BazelOption {
    pub name: &'static str,
    pub kind: BazelKind,
    /// Its default value as text, or `None` if it has none.
    pub default: Option<&'static str>,
    commands: &'static [&'static str],
}

impl BazelOption {
    /// Whether `command` takes the option.
    pub fn takes(&self, command: &str) -> bool {
        self.commands.contains(&command)
    }

    /// Whether `value` leaves the option as it is by default: it's the
    /// default, or empty for an option without one.
    pub fn is_default(&self, value: &str) -> bool {
        match self.default {
            Some(default) => value == default || matches!((self.kind, default, value), (BazelKind::Bool, "true", "1" | "yes") | (BazelKind::Bool, "false", "0" | "no")),
            None => value.is_empty(),
        }
    }
}

/// Bazel's option named `name`, if it has one.
pub fn bazel_option(name: &str) -> Option<&'static BazelOption> {
    TABLE.get(name)
}

static TABLE: LazyLock<HashMap<&'static str, BazelOption>> = LazyLock::new(|| parse_table(include_str!("bazel_options.txt")));

fn parse_table(text: &'static str) -> HashMap<&'static str, BazelOption> {
    let mut sets: Vec<&'static [&'static str]> = Vec::new();
    let mut options = HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if let Some(set) = line.strip_prefix('@') {
            let commands: Vec<&'static str> = set.split(' ').skip(1).collect();
            sets.push(commands.leak());
            continue;
        }
        let option = parse_option(line, &sets);
        options.insert(option.name, option);
    }
    options
}

fn parse_option(line: &'static str, sets: &[&'static [&'static str]]) -> BazelOption {
    let mut fields = line.splitn(4, ' ');
    let mut field = || fields.next().unwrap_or_else(|| panic!("bad line in bazel_options.txt: {line}"));
    let name = field();
    let kind = match field() {
        "bool" => BazelKind::Bool,
        "tristate" => BazelKind::TriState,
        "void" => BazelKind::Expansion,
        "value" => BazelKind::Value,
        "multi" => BazelKind::Multi,
        other => panic!("bad kind '{other}' in bazel_options.txt"),
    };
    let set: usize = field().parse().unwrap_or_else(|_| panic!("bad command set in bazel_options.txt: {line}"));
    let default = match field() {
        "-" => None,
        "''" => Some(""),
        default => Some(default),
    };
    BazelOption { name, kind, default, commands: sets[set] }
}
