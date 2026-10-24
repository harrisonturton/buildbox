//! Bazel's names for things: repositories, packages, labels and target
//! patterns.
//!
//! Each is a few interned ids, so labels are 12 bytes, and compare and hash
//! as integers.

mod pattern;
mod repo;

pub use pattern::{PatternError, TargetPattern};
pub use repo::{RepoMapping, RepoName};

use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use std::fmt;

/// A package: a directory of a repository that has a BUILD file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct PackageId {
    pub repo: RepoName,
    pub path: PathId,
}

/// A target: a name within a package.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct Label {
    pub package: PackageId,
    pub name: Symbol,
}

/// Why text isn't a valid label.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid label '{label}': {reason}")]
pub struct LabelError {
    pub label: String,
    pub reason: String,
}

/// What a label is relative to: the package it appears in, and how that
/// package's repository sees other repositories.
#[derive(Clone, Copy)]
pub struct LabelContext<'a> {
    pub package: PackageId,
    pub mapping: &'a dyn RepoMapping,
}

impl PackageId {
    pub fn new(repo: RepoName, path: PathId) -> PackageId {
        PackageId { repo, path }
    }

    /// A package of the main repository.
    pub fn main(path: &str) -> Result<PackageId, LabelError> {
        let path = parse_package_path(path, path)?;
        Ok(PackageId::new(RepoName::MAIN, path))
    }

    /// The label of a target in this package.
    pub fn label(self, name: Symbol) -> Label {
        Label { package: self, name }
    }
}

impl Label {
    /// Parse a label relative to `context`, as it would be written in a BUILD
    /// or `.bzl` file of the context's package.
    pub fn parse(text: &str, context: &LabelContext<'_>) -> Result<Label, LabelError> {
        let (repo, rest) = split_repo(text, context)?;
        if let (Some(repo), "") = (repo, rest) {
            return Ok(repo_shorthand(text, repo));
        }
        let Some(absolute) = rest.strip_prefix("//") else {
            if repo.is_some() {
                return Err(error(text, "a repository must be followed by '//'"));
            }
            return parse_relative(text, rest, context.package);
        };
        let repo = repo.unwrap_or(context.package.repo);
        let (package, name) = match absolute.split_once(':') {
            Some((package, name)) => (package, name),
            None => (absolute, absolute.rsplit('/').next().unwrap_or(absolute)),
        };
        let path = parse_package_path(text, package)?;
        let name = parse_target_name(text, name)?;
        Ok(PackageId::new(repo, path).label(name))
    }

    /// Parse a label in canonical form: `@@repo//package:name`, or
    /// `//package:name` in the main repository.
    pub fn parse_canonical(text: &str) -> Result<Label, LabelError> {
        let context = LabelContext {
            package: PackageId::new(RepoName::MAIN, PathId::ROOT),
            mapping: &repo::NoMapping,
        };
        if !text.starts_with("@@") && !text.starts_with("//") {
            return Err(error(text, "a canonical label starts with '@@' or '//'"));
        }
        Label::parse(text, &context)
    }

    pub fn repo(self) -> RepoName {
        self.package.repo
    }

    /// The label's form that doesn't depend on any repository mapping:
    /// `@@repo//package:name`, or `@@//package:name` for the main repository.
    pub fn canonical_form(self) -> String {
        format!("@@{}//{}:{}", self.package.repo, self.package.path, self.name)
    }

    /// A label in the same package.
    pub fn sibling(self, name: Symbol) -> Label {
        self.package.label(name)
    }
}

/// The repository a label names, if it names one, and the rest of the text.
fn split_repo<'t>(text: &'t str, context: &LabelContext<'_>) -> Result<(Option<RepoName>, &'t str), LabelError> {
    if let Some(rest) = text.strip_prefix("@@") {
        let (name, rest) = split_repo_name(rest);
        repo::validate(name).map_err(|reason| error(text, &reason))?;
        return Ok((Some(RepoName::new(name)), rest));
    }
    let Some(rest) = text.strip_prefix('@') else {
        return Ok((None, text));
    };
    let (name, rest) = split_repo_name(rest);
    if name.is_empty() {
        return Ok((Some(RepoName::MAIN), rest));
    }
    repo::validate(name).map_err(|reason| error(text, &reason))?;
    let Some(repo) = context.mapping.resolve(name) else {
        return Err(error(text, &format!("no repository visible as '@{name}' from {}", context.package.repo.describe())));
    };
    Ok((Some(repo), rest))
}

/// `@repo` alone means `@repo//:repo`, named as written.
fn repo_shorthand(text: &str, repo: RepoName) -> Label {
    let name = text.trim_start_matches('@');
    PackageId::new(repo, PathId::ROOT).label(Symbol::intern(name))
}

fn split_repo_name(text: &str) -> (&str, &str) {
    match text.find("//") {
        Some(end) => (&text[..end], &text[end..]),
        None => (text, ""),
    }
}

fn parse_relative(text: &str, rest: &str, package: PackageId) -> Result<Label, LabelError> {
    let name = rest.strip_prefix(':').unwrap_or(rest);
    if rest.contains(':') && !rest.starts_with(':') {
        return Err(error(text, "relative labels can't name another package; use '//package:name'"));
    }
    let name = parse_target_name(text, name)?;
    Ok(package.label(name))
}

fn parse_package_path(text: &str, path: &str) -> Result<PathId, LabelError> {
    if path.ends_with('/') {
        return Err(error(text, "package names may not end with '/'"));
    }
    if let Some(bad) = path.chars().find(|&c| !is_package_char(c)) {
        return Err(error(text, &format!("package names may not contain '{bad}'")));
    }
    PathId::parse(path).map_err(|_| error(text, "package names may not contain '//', '.' or '..' segments, or start with '/'"))
}

fn parse_target_name(text: &str, name: &str) -> Result<Symbol, LabelError> {
    if name.is_empty() {
        return Err(error(text, "empty target name"));
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err(error(text, "target names may not start or end with '/', or contain '//'"));
    }
    let bad_segment = name.split('/').any(|segment| segment == ".." || segment == ".");
    if bad_segment && name != "." {
        return Err(error(text, "target names may not contain '.' or '..' segments"));
    }
    if let Some(bad) = name.chars().find(|&c| c == '\0' || c == '\n' || c == '\r') {
        return Err(error(text, &format!("target names may not contain {bad:?}")));
    }
    Ok(Symbol::intern(name))
}

fn is_package_char(c: char) -> bool {
    c.is_alphanumeric() || "/-._ !#$%&()*+,;<=>?[]^`{|}~@".contains(c)
}

fn error(label: &str, reason: &str) -> LabelError {
    LabelError {
        label: label.to_string(),
        reason: reason.to_string(),
    }
}

impl fmt::Display for PackageId {
    /// Formats as `//path` in the main repository, `@@repo//path` otherwise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.repo.is_main() {
            write!(f, "@@{}", self.repo)?;
        }
        write!(f, "//{}", self.path)
    }
}

impl fmt::Debug for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Label {
    /// Formats as `//package:name` in the main repository,
    /// `@@repo//package:name` otherwise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.package, self.name)
    }
}

impl fmt::Debug for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[cfg(test)]
mod tests;
