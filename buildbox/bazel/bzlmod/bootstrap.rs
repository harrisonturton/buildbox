//! Running `@bazel_tools`'s repository rules before the dependency graph
//! exists, as the root module's `git_override` and `archive_override` do.
//!
//! Such a rule fetches a module whose MODULE.bazel the graph is resolved
//! from, so neither its `.bzl` file nor the labels in its attributes can be
//! resolved with mappings that come from the graph. As in Bazel, the file is
//! loaded seeing only `@bazel_tools`, and labels in the attributes may name
//! only the main repository.

use crate::config::BzlmodConfig;
use bazel_label::{Label, RepoMappingTable, RepoName};
use bazel_loading::{BzlModule, DirLoader, blocking};
use intern::Symbol;
use skycodec::{Decode, Encode, Fingerprinter};
use skyframe::{Ctx, Key, SkyError};
use std::collections::HashMap;
use std::sync::Arc;

/// `@bazel_tools`'s `.bzl` file defining `git_repository`.
pub const GIT_BZL: &str = "@@bazel_tools//tools/build_defs/repo:git.bzl";

/// `@bazel_tools`'s `.bzl` file defining `http_archive`.
pub const HTTP_BZL: &str = "@@bazel_tools//tools/build_defs/repo:http.bzl";

/// Loads a `.bzl` file of `@bazel_tools` from Bazel's installation, seeing
/// only `@bazel_tools` and none of the builtins other `.bzl` files see.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BootstrapBzlKey(pub Label);

impl Key for BootstrapBzlKey {
    type Value = BzlModule;
    const KIND: &'static str = "BZL_LOAD_BOOTSTRAP";

    async fn compute(&self, ctx: &Ctx) -> Result<BzlModule, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        let Some(dir) = config.bazel_tools.clone() else {
            return Err(SkyError::new(crate::resolve::NO_BAZEL_TOOLS));
        };
        let label = self.0;
        let mut fingerprinter = Fingerprinter::new();
        fingerprinter.bytes(dir.display().to_string().as_bytes());
        fingerprinter.bytes(label.canonical_form().as_bytes());
        let fingerprint = fingerprinter.finish();
        let loaded = blocking(move || DirLoader::new("bootstrap", dir, bazel_tools_mapping()).load(label)).await?;
        let module = loaded.map_err(|error| SkyError::new(format!("loading {label}: {error}")))?;
        Ok(BzlModule {
            label,
            module,
            fingerprint,
            repos: vec![RepoName::new("bazel_tools")],
            loads: Vec::new(),
        })
    }
}

/// How `@bazel_tools` sees repositories while the graph is resolved: only
/// itself.
fn bazel_tools_mapping() -> RepoMappingTable {
    let bazel_tools = RepoName::new("bazel_tools");
    RepoMappingTable::new(bazel_tools, vec![(Symbol::intern("bazel_tools"), bazel_tools)])
}

/// How the main repository is seen while the graph is resolved: only as
/// itself, by the name the root module gives its own repository.
fn main_mapping(root_repo_name: &str) -> RepoMappingTable {
    let mut entries = Vec::new();
    if !root_repo_name.is_empty() {
        entries.push((Symbol::intern(root_repo_name), RepoName::MAIN));
    }
    RepoMappingTable::new(RepoName::MAIN, entries)
}

/// The mappings a bootstrap rule runs with: `@bazel_tools`'s, and the main
/// repository's, which its attributes are relative to.
pub fn mappings(root_repo_name: &str) -> HashMap<RepoName, Arc<RepoMappingTable>> {
    HashMap::from([
        (RepoName::new("bazel_tools"), Arc::new(bazel_tools_mapping())),
        (RepoName::MAIN, Arc::new(main_mapping(root_repo_name))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_label::RepoMapping;

    #[test]
    fn the_main_repository_is_seen_by_the_root_modules_name_for_it() {
        let mapping = main_mapping("carbon");
        assert_eq!(mapping.resolve("carbon"), Some(RepoName::MAIN));
        assert_eq!(mapping.resolve("llvm-raw"), None);
    }

    #[test]
    fn bazel_tools_sees_only_itself() {
        let mapping = bazel_tools_mapping();
        assert_eq!(mapping.resolve("bazel_tools"), Some(RepoName::new("bazel_tools")));
        assert_eq!(mapping.resolve("rules_cc"), None);
    }
}
