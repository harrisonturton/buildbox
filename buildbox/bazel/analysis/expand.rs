//! Expanding `$(location)` functions and make variables in strings.

use crate::artifact::Artifact;
use std::collections::BTreeSet;

/// Expand `$(name)`, `$(function argument)`, `$X` and `$$` in `text`,
/// asking `lookup` for the value of each variable or function call.
pub fn expand(text: &str, lookup: &mut dyn FnMut(&str, Option<&str>) -> anyhow::Result<String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let mut chars = after.chars();
        match chars.next() {
            None => anyhow::bail!("'$' at the end of '{text}'; write '$$' for a literal '$'"),
            Some('$') => {
                out.push('$');
                rest = &after[1..];
            }
            Some('(') => {
                let Some(end) = after.find(')') else {
                    anyhow::bail!("unterminated $( in '{text}'");
                };
                let inner = after[1..end].trim();
                let (name, argument) = match inner.split_once(char::is_whitespace) {
                    Some((name, argument)) => (name, Some(argument.trim())),
                    None => (inner, None),
                };
                out.push_str(&lookup(name, argument)?);
                rest = &after[end + 1..];
            }
            Some(c) => {
                let mut name = [0u8; 4];
                out.push_str(&lookup(c.encode_utf8(&mut name), None)?);
                rest = &after[c.len_utf8()..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// How `$(location)` writes paths.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LocationStyle {
    /// Relative to the execution root, as actions see them.
    Exec,
    /// Relative to the runfiles tree, as executables see them.
    Root,
}

/// The value of a location function, such as `execpaths`, applied to the
/// files of a target, or `None` if `function` isn't one. As in Bazel,
/// the paths are sorted, without duplicates, written so that a shell takes
/// each as a path (`./name` for a single relative name), and quoted for
/// the shell.
pub fn location(function: &str, files: &[Artifact], style: LocationStyle, what: &str) -> Option<anyhow::Result<String>> {
    let (path, multiple): (fn(&Artifact) -> String, bool) = match function {
        "location" if style == LocationStyle::Exec => (exec_path, false),
        "location" => (root_path, false),
        "locations" if style == LocationStyle::Exec => (exec_path, true),
        "locations" => (root_path, true),
        "execpath" => (exec_path, false),
        "execpaths" => (exec_path, true),
        "rootpath" => (root_path, false),
        "rootpaths" => (root_path, true),
        "rlocationpath" => (runfiles_path, false),
        "rlocationpaths" => (runfiles_path, true),
        _ => return None,
    };
    let shown = if multiple { "$(locations)" } else { "$(location)" };
    let paths: BTreeSet<String> = files.iter().map(|file| callable(path(file))).collect();
    if paths.is_empty() {
        return Some(Err(anyhow::anyhow!("label '{what}' in {shown} expression expands to no files")));
    }
    if !multiple && paths.len() > 1 {
        let listed: Vec<&String> = paths.iter().take(MAX_PATHS_SHOWN).collect();
        return Some(Err(anyhow::anyhow!(
            "label '{what}' in $(location) expression expands to more than one file, please use $(locations {what}) instead.  Files (at most {MAX_PATHS_SHOWN} shown) are: [{}]",
            listed.iter().map(|path| path.as_str()).collect::<Vec<_>>().join(", ")
        )));
    }
    let escaped: Vec<String> = paths.iter().map(|path| crate::shell::escape(path)).collect();
    Some(Ok(escaped.join(" ")))
}

/// How many paths an error about too many lists.
const MAX_PATHS_SHOWN: usize = 5;

/// `path` written so a shell takes it as a path rather than a command, as
/// Bazel's `PathFragment.getCallablePathString` writes it.
fn callable(path: String) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    if path.contains('/') {
        return path;
    }
    format!("./{path}")
}

fn exec_path(artifact: &Artifact) -> String {
    artifact.exec_path.to_string()
}

fn root_path(artifact: &Artifact) -> String {
    artifact.short_path()
}

fn runfiles_path(artifact: &Artifact) -> String {
    artifact.runfiles_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_variables_and_functions() {
        let mut lookup = |name: &str, argument: Option<&str>| -> anyhow::Result<String> {
            Ok(match argument {
                Some(argument) => format!("<{name}:{argument}>"),
                None => format!("<{name}>"),
            })
        };
        let expanded = expand("a $(location //x) $@ $$b $(BINDIR)", &mut lookup).unwrap();
        assert_eq!(expanded, "a <location://x> <@> $b <BINDIR>");
        assert!(expand("trailing $", &mut lookup).is_err());
        assert!(expand("$(open", &mut lookup).is_err());
    }

    #[test]
    fn callable_paths_name_a_path() {
        assert_eq!(callable("data.txt".to_string()), "./data.txt");
        assert_eq!(callable("pkg/data.txt".to_string()), "pkg/data.txt");
        assert_eq!(callable(String::new()), ".");
    }
}
