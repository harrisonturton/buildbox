//! Expanding `$(location)` functions and make variables in strings.

use crate::artifact::Artifact;

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
/// files of a target, or `None` if `function` isn't one.
pub fn location(function: &str, files: &[Artifact], style: LocationStyle, what: &str) -> Option<anyhow::Result<String>> {
    let (path, single): (fn(&Artifact) -> String, bool) = match function {
        "location" if style == LocationStyle::Exec => (exec_path, true),
        "location" => (root_path, true),
        "locations" if style == LocationStyle::Exec => (exec_path, false),
        "locations" => (root_path, false),
        "execpath" => (exec_path, true),
        "execpaths" => (exec_path, false),
        "rootpath" => (root_path, true),
        "rootpaths" => (root_path, false),
        "rlocationpath" => (runfiles_path, true),
        "rlocationpaths" => (runfiles_path, false),
        _ => return None,
    };
    if single && files.len() != 1 {
        return Some(Err(anyhow::anyhow!(
            "{what} expands to {} files, but $({function}) expects exactly one; use $({function}s)",
            files.len()
        )));
    }
    let paths: Vec<String> = files.iter().map(path).collect();
    Some(Ok(paths.join(" ")))
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
}
