//! The `external` directory of a workspace's output base, where Bazel keeps
//! each repository it fetched and where tools such as IDEs look for their
//! files: `<output_base>/external/<canonical name>`.
//!
//! bb fetches repositories into a directory shared by every workspace, so
//! each workspace's `external` holds links to the trees its own graph
//! resolved, made again after each command, since another workspace may
//! have fetched them.

use bazel_bzlmod::fetch_repo::{FetchSpecKey, RepoFetchKey};
use bazel_bzlmod::resolve::DepGraphKey;
use bazel_label::RepoName;
use skyframe::{Graph, Session};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Link each repository the graph has already fetched at its current
/// revision as `external/<canonical name>` in `output_base`. Repositories
/// it hasn't fetched aren't fetched, and links it no longer resolves are
/// kept, as Bazel keeps their directories.
pub async fn link_fetched(graph: &Graph, output_base: &Path) {
    let fetched = {
        let session = graph.session().await;
        fetched(&session)
    };
    let links: Vec<(String, PathBuf)> = fetched.into_iter().map(|(repo, dir)| (repo.as_str().to_string(), dir)).collect();
    let external = output_base.join("external");
    let linked = tokio::task::spawn_blocking(move || link_all(&external, &links)).await;
    if let Err(error) = linked {
        tracing::warn!("failed to link external repositories: {error}");
    }
}

/// The directory of each repository the session's graph has fetched: those
/// fetched by name, and the modules resolution fetched to read their
/// `MODULE.bazel`, such as overridden ones, with the specs the module graph
/// gives them now.
fn fetched(session: &Session) -> HashMap<RepoName, PathBuf> {
    let mut fetched = HashMap::new();
    if let Some((_, graph)) = session.evaluated::<DepGraphKey>().pop() {
        let specs: HashMap<RepoName, _> = graph.modules.iter().filter_map(|module| Some((module.repo, module.spec.as_ref()?))).collect();
        for (key, repo) in session.evaluated::<FetchSpecKey>() {
            if specs.get(&key.repo) == Some(&&key.spec) {
                fetched.insert(key.repo, repo.path());
            }
        }
    }
    for (key, repo) in session.evaluated::<RepoFetchKey>() {
        fetched.insert(key.0, repo.path());
    }
    fetched
}

/// Make `external/<name>` a link to each repository's directory.
fn link_all(external: &Path, links: &[(String, PathBuf)]) {
    if links.is_empty() {
        return;
    }
    if let Err(error) = std::fs::create_dir_all(external) {
        tracing::warn!("failed to create {}: {error}", external.display());
        return;
    }
    for (name, target) in links {
        if let Err(error) = link(external, name, target) {
            tracing::warn!("failed to link {} to {}: {error}", external.join(name).display(), target.display());
        }
    }
}

/// Make `dir/name` a symbolic link to `target`, unless it already is one.
/// The link is made beside it and renamed over it, so a reader, or another
/// command linking it at the same time, never sees it missing or broken.
fn link(dir: &Path, name: &str, target: &Path) -> std::io::Result<()> {
    let path = dir.join(name);
    if std::fs::read_link(&path).is_ok_and(|current| current == target) {
        return Ok(());
    }
    let temporary = dir.join(temporary_name(name));
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(target, &temporary)?;
    let renamed = std::fs::rename(&temporary, &path);
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    renamed
}

/// A name for a new link to `name`, unique among this process's links and
/// other processes'. Canonical repository names never start with a dot.
fn temporary_name(name: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!(".{name}.{}.{count}.tmp", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{workspaces, write};
    use bazel_fs::{FileContentKey, Root, RootedPath};
    use intern::PathId;

    /// A workspace depending on module `lib` at `lib_dir` through a
    /// `local_path_override`.
    fn workspace_using(root: &Path, lib_dir: &Path) {
        let module = format!(
            "module(name = 'app')\nbazel_dep(name = 'lib')\nlocal_path_override(module_name = 'lib', path = '{}')\n",
            lib_dir.display()
        );
        write(&root.join("MODULE.bazel"), &module);
    }

    fn module_lib(dir: &Path) {
        write(&dir.join("MODULE.bazel"), "module(name = 'lib')\n");
        write(&dir.join("BUILD.bazel"), "");
    }

    /// Fetch `@@lib+` in the workspace's graph, as a command would, then
    /// link what's fetched.
    async fn fetch_lib(graph: &Graph, output_base: &Path) {
        let session = graph.session().await;
        session.evaluate(&RepoFetchKey(RepoName::new("lib+"))).await.unwrap();
        drop(session);
        link_fetched(graph, output_base).await;
    }

    #[tokio::test]
    async fn each_workspace_links_the_repositories_it_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let (first_lib, second_lib) = (dir.path().join("lib1"), dir.path().join("lib2"));
        module_lib(&first_lib);
        module_lib(&second_lib);
        let (first_root, second_root) = (dir.path().join("first"), dir.path().join("second"));
        workspace_using(&first_root, &first_lib);
        workspace_using(&second_root, &second_lib);
        let workspaces = workspaces(&dir.path().join("cache"));
        let (first, second) = (workspaces.get(&first_root), workspaces.get(&second_root));
        let (first_base, second_base) = (dir.path().join("base1"), dir.path().join("base2"));

        fetch_lib(&first.graph, &first_base).await;
        fetch_lib(&second.graph, &second_base).await;
        assert_eq!(std::fs::read_link(first_base.join("external/lib+")).unwrap(), first_lib);
        assert_eq!(std::fs::read_link(second_base.join("external/lib+")).unwrap(), second_lib);
        assert!(first_base.join("external/lib+/MODULE.bazel").is_file());

        workspace_using(&first_root, &second_lib);
        let module_file = FileContentKey(RootedPath::new(Root::Workspace, PathId::parse("MODULE.bazel").unwrap()));
        first.graph.invalidate(&[module_file]).await;
        fetch_lib(&first.graph, &first_base).await;
        assert_eq!(std::fs::read_link(first_base.join("external/lib+")).unwrap(), second_lib, "relinked to the version resolved now");
    }

    #[tokio::test]
    async fn modules_resolution_fetched_are_linked() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        module_lib(&lib);
        let root = dir.path().join("app");
        workspace_using(&root, &lib);
        let workspaces = workspaces(&dir.path().join("cache"));
        let workspace = workspaces.get(&root);
        let output_base = dir.path().join("base");

        workspace.graph.session().await.evaluate(&DepGraphKey).await.unwrap();
        link_fetched(&workspace.graph, &output_base).await;

        assert_eq!(std::fs::read_link(output_base.join("external/lib+")).unwrap(), lib);
    }

    #[tokio::test]
    async fn links_lost_are_made_again_without_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib");
        module_lib(&lib);
        let root = dir.path().join("app");
        workspace_using(&root, &lib);
        let workspaces = workspaces(&dir.path().join("cache"));
        let workspace = workspaces.get(&root);
        let output_base = dir.path().join("base");
        fetch_lib(&workspace.graph, &output_base).await;

        std::fs::remove_dir_all(output_base.join("external")).unwrap();
        link_fetched(&workspace.graph, &output_base).await;
        assert_eq!(std::fs::read_link(output_base.join("external/lib+")).unwrap(), lib);
    }

    #[test]
    fn links_replace_others_and_leave_nothing_beside_them() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("old"), dir.path().join("new"));
        link(dir.path(), "repo+", &old).unwrap();
        link(dir.path(), "repo+", &new).unwrap();
        link(dir.path(), "repo+", &new).unwrap();

        assert_eq!(std::fs::read_link(dir.path().join("repo+")).unwrap(), new);
        let mut names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names, ["repo+"]);
    }
}
