use bazel_fs::{DirListingKey, FileContentKey, Root, RootedPath};
use bazel_label::{Label, PackageId, RepoMappingTable, RepoName};
use bazel_loading::{PackageKey, Repositories, RepositoryResolver, packages_beneath};
use futures::future::BoxFuture;
use intern::{PathId, Symbol};
use skyframe::{Ctx, Engine, Globals, Graph, SkyError};
use std::path::Path;
use std::sync::Arc;

struct MainOnly;

impl RepositoryResolver for MainOnly {
    fn root<'a>(&'a self, _ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Root, SkyError>> {
        Box::pin(async move {
            if repo.is_main() {
                return Ok(Root::Workspace);
            }
            Err(SkyError::new(format!("no repository @@{repo}")))
        })
    }

    fn mapping<'a>(&'a self, _ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<RepoMappingTable, SkyError>> {
        Box::pin(async move { Ok(RepoMappingTable::new(repo, vec![(Symbol::intern(""), RepoName::MAIN)])) })
    }
}

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[
            ("MODULE.bazel", ""),
            ("defs/BUILD", ""),
            (
                "defs/rules.bzl",
                "def _impl(ctx):\n    pass\nlib = rule(implementation = _impl, attrs = {'srcs': attr.label_list(allow_files = True), 'deps': attr.label_list()})\n",
            ),
            ("app/BUILD.bazel", "load('//defs:rules.bzl', 'lib')\nlib(name = 'app', srcs = glob(['*.c']), deps = ['//app/sub'])\n"),
            ("app/main.c", ""),
            ("app/sub/BUILD", "filegroup(name = 'sub')\n"),
            ("notapackage/file.txt", ""),
            ("bazel-out/BUILD", "this is a convenience symlink, never loaded"),
        ],
    );
    dir
}

fn graph(engine: &Engine, root: &Path) -> Graph {
    let globals = Globals::new()
        .with(bazel_fs::workspace(root))
        .with(Arc::new(Repositories(Arc::new(MainOnly))))
        .with(Arc::new(bazel_loading::BuiltinsDir(None)));
    engine.graph(globals)
}

fn package(path: &str) -> PackageId {
    PackageId::main(path).unwrap()
}

#[tokio::test]
async fn packages_load_through_the_graph() {
    let dir = workspace();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    let session = graph.session().await;

    let app = session.evaluate(&PackageKey(package("app"))).await.unwrap();
    assert_eq!(app.0.errors, Vec::<String>::new());
    let names: Vec<&str> = app.0.targets.iter().map(|t| t.name().as_str()).collect();
    assert_eq!(names, ["BUILD.bazel", "app", "main.c"]);
    assert_eq!(app.0.loads, [Label::parse_canonical("//defs:rules.bzl").unwrap()]);

    let missing = session.evaluate(&PackageKey(package("notapackage"))).await.unwrap_err();
    assert!(missing.message().starts_with("no such package 'notapackage'"), "{missing}");
}

#[tokio::test]
async fn packages_are_found_beneath_directories() {
    let dir = workspace();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    let session = graph.session().await;
    let packages = packages_beneath(&session, RepoName::MAIN, PathId::ROOT).await.unwrap();
    let paths: Vec<String> = packages.iter().map(|p| p.path.to_string()).collect();
    assert_eq!(paths, ["app", "app/sub", "defs"]);
}

#[tokio::test]
async fn editing_a_bzl_file_reloads_the_packages_that_use_it() {
    let dir = workspace();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    let first = graph.session().await.evaluate(&PackageKey(package("app"))).await.unwrap();
    assert!(first.0.errors.is_empty());

    write(dir.path(), &[("defs/rules.bzl", "lib = 1\n")]);
    graph
        .invalidate(&[FileContentKey(RootedPath::new(Root::Workspace, PathId::parse("defs/rules.bzl").unwrap()))])
        .await;
    let reloaded = graph.session().await.evaluate(&PackageKey(package("app"))).await.unwrap();
    assert_eq!(reloaded.0.errors.len(), 1);
    assert!(reloaded.0.errors[0].contains("not supported on type `int`"), "{:?}", reloaded.0.errors);
}

#[tokio::test]
async fn worktrees_share_loaded_packages() {
    let first_dir = workspace();
    let second_dir = workspace();
    let engine = Engine::default();
    let first = graph(&engine, first_dir.path());
    let second = graph(&engine, second_dir.path());

    first.session().await.evaluate(&PackageKey(package("app"))).await.unwrap();
    second.session().await.evaluate(&PackageKey(package("app"))).await.unwrap();

    let stats = engine.stats();
    let row = |name: &str| stats.kinds.iter().find(|kind| kind.name == name).cloned().unwrap();
    assert_eq!(row("PACKAGE").computed, 1, "the second worktree reuses the first's package");
    assert_eq!(row("BZL_LOAD").computed, 1);
    assert_eq!(row("PACKAGE").reused, 1);
    assert_eq!(row("PACKAGE").values.values, 1, "the package is stored once");
    let _ = DirListingKey(RootedPath::new(Root::Workspace, PathId::ROOT));
}
