//! Target patterns, as given on the command line: `//foo:bar`,
//! `//foo:all`, `//foo/...`.

use crate::{Label, LabelContext, LabelError, PackageId, RepoName};
use intern::PathId;
use std::fmt;

/// A set of targets named on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetPattern {
    /// One target, `//foo:bar`.
    Target(Label),
    /// The targets of one package: `//foo:all` for its rules, or `//foo:*`
    /// and `//foo:all-targets` for every target, including files.
    Package { package: PackageId, rules_only: bool },
    /// The targets of every package beneath a directory: `//foo/...`, which
    /// is short for `//foo/...:all`, or `//foo/...:*`.
    Recursive { repo: RepoName, prefix: PathId, rules_only: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatternError {
    #[error(transparent)]
    Label(#[from] LabelError),

    #[error("invalid target pattern '{0}'")]
    Invalid(String),
}

impl TargetPattern {
    /// Parse a pattern. Patterns not starting with `//` or `@` are relative
    /// to `context`'s package, which is the working directory's.
    pub fn parse(text: &str, context: &LabelContext<'_>) -> Result<TargetPattern, PatternError> {
        let absolute = to_absolute(text, context)?;
        let (package_part, target) = match absolute.rsplit_once(':') {
            Some((package, target)) if !package.ends_with(':') => (package.to_string(), Some(target.to_string())),
            _ => (absolute.clone(), None),
        };

        if let Some(prefix) = package_part.strip_suffix("...") {
            let rules_only = match target.as_deref() {
                None | Some("all") => true,
                Some("*") | Some("all-targets") => false,
                Some(_) => return Err(PatternError::Invalid(text.to_string())),
            };
            let prefix = match prefix.strip_suffix('/') {
                Some(stripped) if !prefix.ends_with("//") => stripped,
                _ => prefix,
            };
            let package = parse_package(text, prefix, context)?;
            return Ok(TargetPattern::Recursive {
                repo: package.repo,
                prefix: package.path,
                rules_only,
            });
        }

        match target.as_deref() {
            Some("all") => {
                let package = parse_package(text, &package_part, context)?;
                return Ok(TargetPattern::Package { package, rules_only: true });
            }
            Some("*") | Some("all-targets") => {
                let package = parse_package(text, &package_part, context)?;
                return Ok(TargetPattern::Package { package, rules_only: false });
            }
            _ => {}
        }
        Ok(TargetPattern::Target(Label::parse(&absolute, context)?))
    }

    /// Whether the pattern names exactly one target.
    pub fn is_single_target(&self) -> bool {
        matches!(self, TargetPattern::Target(_))
    }
}

/// Make a pattern start with `//` or `@`, interpreting a relative one in the
/// context's package.
fn to_absolute(text: &str, context: &LabelContext<'_>) -> Result<String, PatternError> {
    if text.starts_with("//") || text.starts_with('@') {
        return Ok(text.to_string());
    }
    if text.is_empty() {
        return Err(PatternError::Invalid(text.to_string()));
    }
    let base = context.package;
    let repo = if base.repo.is_main() { String::new() } else { format!("@@{}", base.repo) };
    let joined = |rest: &str| {
        if base.path.is_root() {
            format!("{repo}//{rest}")
        } else if rest.is_empty() {
            format!("{repo}//{}", base.path)
        } else {
            format!("{repo}//{}/{rest}", base.path)
        }
    };
    if let Some(target) = text.strip_prefix(':') {
        return Ok(format!("{}:{target}", joined("")));
    }
    Ok(joined(text))
}

fn parse_package(text: &str, package: &str, context: &LabelContext<'_>) -> Result<PackageId, PatternError> {
    let label_text = format!("{package}:__pattern__");
    let label = Label::parse(&label_text, context).map_err(|error| LabelError {
        label: text.to_string(),
        reason: error.reason,
    })?;
    Ok(label.package)
}

impl fmt::Display for TargetPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetPattern::Target(label) => write!(f, "{label}"),
            TargetPattern::Package { package, rules_only } => {
                write!(f, "{package}:{}", if *rules_only { "all" } else { "*" })
            }
            TargetPattern::Recursive { repo, prefix, rules_only } => {
                let package = PackageId::new(*repo, *prefix);
                let separator = if prefix.is_root() { "" } else { "/" };
                write!(f, "{package}{separator}...{}", if *rules_only { "" } else { ":*" })
            }
        }
    }
}
