//! Where repositories are, before Bzlmod resolves them: the main repository
//! and Bazel's built-in `@bazel_tools`.

use bazel_fs::Root;
use bazel_label::{RepoMappingTable, RepoName};
use bazel_loading::RepositoryResolver;
use futures::future::BoxFuture;
use intern::Symbol;
use skyframe::{Ctx, SkyError};
use std::path::{Path, PathBuf};

/// Resolves the main repository, and `@bazel_tools` if its files were found.
pub struct BuiltinRepositories {
    pub bazel_tools: Option<PathBuf>,
}

impl BuiltinRepositories {
    fn bazel_tools_root(&self) -> Result<Root, SkyError> {
        match &self.bazel_tools {
            Some(dir) => Ok(Root::Dir(Symbol::intern(&dir.display().to_string()))),
            None => Err(SkyError::new(
                "@bazel_tools isn't available: no Bazel installation was found to take it from; set BB_BAZEL_TOOLS",
            )),
        }
    }
}

impl RepositoryResolver for BuiltinRepositories {
    fn root<'a>(&'a self, _ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Root, SkyError>> {
        Box::pin(async move {
            match repo.as_str() {
                "" => Ok(Root::Workspace),
                "bazel_tools" => self.bazel_tools_root(),
                other => Err(SkyError::new(format!("no repository named '@@{other}'"))),
            }
        })
    }

    fn mapping<'a>(&'a self, _ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<RepoMappingTable, SkyError>> {
        Box::pin(async move {
            let entries = vec![
                (Symbol::intern(""), RepoName::MAIN),
                (Symbol::intern("bazel_tools"), RepoName::new("bazel_tools")),
            ];
            Ok(RepoMappingTable::new(repo, entries))
        })
    }
}

/// Find `@bazel_tools`'s files: in `BB_BAZEL_TOOLS`, or the `embedded_tools`
/// of an installed Bazel, preferring the version the workspace asks for in
/// `.bazelversion`.
pub fn find_bazel_tools(home: Option<&Path>, wanted_version: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BB_BAZEL_TOOLS") {
        return Some(PathBuf::from(dir));
    }
    let cache = home?.join(".cache/bazel");
    let mut candidates = Vec::new();
    for user_dir in std::fs::read_dir(&cache).ok()?.flatten() {
        let Ok(installs) = std::fs::read_dir(user_dir.path().join("install")) else {
            continue;
        };
        for install in installs.flatten() {
            let tools = install.path().join("embedded_tools");
            if !tools.join("tools").is_dir() {
                continue;
            }
            let label = std::fs::read_to_string(install.path().join("build-label.txt")).unwrap_or_default();
            let modified = install.metadata().and_then(|m| m.modified()).ok();
            candidates.push((label.trim().to_string(), modified, tools));
        }
    }
    if let Some(version) = wanted_version {
        if let Some((_, _, tools)) = candidates.iter().find(|(label, _, _)| label == version) {
            return Some(tools.clone());
        }
    }
    candidates.sort_by_key(|(_, modified, _)| *modified);
    candidates.pop().map(|(_, _, tools)| tools)
}
