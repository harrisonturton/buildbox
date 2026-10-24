//! Repository names, and how repositories see each other.

use intern::Symbol;
use skycodec::{Decode, Encode};
use std::fmt;

/// A repository's canonical name: unique in the build, such as
/// `rules_rust+`. The main repository's is empty.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct RepoName(Symbol);

impl Default for RepoName {
    fn default() -> RepoName {
        RepoName::MAIN
    }
}

impl RepoName {
    pub const MAIN: RepoName = RepoName(Symbol::EMPTY);

    pub fn new(name: &str) -> RepoName {
        RepoName(Symbol::intern(name))
    }

    pub fn as_str(self) -> &'static str {
        self.0.as_str()
    }

    pub fn symbol(self) -> Symbol {
        self.0
    }

    pub fn is_main(self) -> bool {
        self == RepoName::MAIN
    }

    /// The name Bazel gives a repository that `from` can't see as
    /// `@apparent`: a name no real repository has, so using it fails.
    pub fn unknown(apparent: &str, from: RepoName) -> RepoName {
        RepoName::new(&format!("[unknown repo '{apparent}' requested from @@{from}]"))
    }

    pub fn is_unknown(self) -> bool {
        self.as_str().starts_with("[unknown repo")
    }

    /// The repository, for messages: `the main repository` or `@@name`.
    pub fn describe(self) -> String {
        if self.is_main() {
            "the main repository".to_string()
        } else {
            format!("@@{self}")
        }
    }
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@@{self}")
    }
}

/// How one repository's labels name other repositories: maps the apparent
/// names it uses, as in `@rules_rust//...`, to canonical names.
pub trait RepoMapping: Send + Sync {
    fn resolve(&self, apparent: &str) -> Option<RepoName>;
}

/// A repository mapping as a table, sorted by apparent name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoMappingTable {
    /// The repository whose view this is.
    pub repo: RepoName,
    pub entries: Vec<(Symbol, RepoName)>,
}

impl RepoMappingTable {
    pub fn new(repo: RepoName, mut entries: Vec<(Symbol, RepoName)>) -> RepoMappingTable {
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries.dedup_by(|a, b| a.0 == b.0);
        RepoMappingTable { repo, entries }
    }

    /// The apparent name `repo` has in this mapping, if it's visible.
    pub fn apparent_name(&self, repo: RepoName) -> Option<Symbol> {
        self.entries.iter().find(|(_, canonical)| *canonical == repo).map(|(apparent, _)| *apparent)
    }
}

impl RepoMapping for RepoMappingTable {
    fn resolve(&self, apparent: &str) -> Option<RepoName> {
        let apparent = Symbol::get(apparent)?;
        let index = self.entries.binary_search_by(|(name, _)| name.cmp(&apparent)).ok()?;
        Some(self.entries[index].1)
    }
}

/// A mapping that sees no other repositories.
pub(crate) struct NoMapping;

impl RepoMapping for NoMapping {
    fn resolve(&self, _apparent: &str) -> Option<RepoName> {
        None
    }
}

impl<F: Fn(&str) -> Option<RepoName> + Send + Sync> RepoMapping for F {
    fn resolve(&self, apparent: &str) -> Option<RepoName> {
        self(apparent)
    }
}

/// Check a repository name's characters.
pub(crate) fn validate(name: &str) -> Result<(), String> {
    if let Some(bad) = name.chars().find(|&c| !(c.is_ascii_alphanumeric() || "-._+~".contains(c))) {
        return Err(format!("repository names may not contain '{bad}'"));
    }
    Ok(())
}
