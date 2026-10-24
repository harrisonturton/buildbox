//! Repository names, and how repositories see each other.

use intern::Symbol;
use skycodec::{Decode, Encode};
use std::fmt;

/// A repository's canonical name: unique in the build, such as
/// `rules_rust+`. The main repository's is empty.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct RepoName(Symbol);

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
