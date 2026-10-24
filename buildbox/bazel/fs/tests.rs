use super::*;
use skyframe::{Engine, Globals, Graph};

fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for file in [
        "pkg/BUILD",
        "pkg/a.cc",
        "pkg/b.cc",
        "pkg/.hidden.cc",
        "pkg/sub/c.cc",
        "pkg/sub/deep/d.cc",
        "pkg/subpkg/BUILD.bazel",
        "pkg/subpkg/e.cc",
    ] {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, file).unwrap();
    }
    dir
}

fn graph(engine: &Engine, root: &Path) -> Graph {
    engine.graph(Globals::new().with(workspace(root)))
}

fn path(text: &str) -> PathId {
    PathId::parse(text).unwrap()
}

fn glob(include: &[&str], exclude: &[&str], exclude_directories: bool) -> Glob {
    Glob {
        package: RootedPath::new(Root::Workspace, path("pkg")),
        include: include.iter().map(|p| GlobPattern::parse(p).unwrap()).collect(),
        exclude: exclude.iter().map(|p| GlobPattern::parse(p).unwrap()).collect(),
        exclude_directories,
    }
}

async fn matches(graph: &Graph, key: Glob) -> Vec<String> {
    let result = graph.session().await.evaluate(&key).await.unwrap();
    result.matches.iter().map(|p| p.to_string()).collect()
}

#[tokio::test]
async fn file_keys_read_the_disk() {
    let dir = tree();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    let session = graph.session().await;

    let state = session.evaluate(&FileStateKey(RootedPath::new(Root::Workspace, path("pkg/a.cc")))).await.unwrap();
    assert_eq!(*state, FileState::File { size: 8, executable: false });
    let missing = session.evaluate(&FileStateKey(RootedPath::new(Root::Workspace, path("nope")))).await.unwrap();
    assert_eq!(*missing, FileState::Missing);

    let content = session.evaluate(&FileContentKey(RootedPath::new(Root::Workspace, path("pkg/BUILD")))).await.unwrap();
    assert_eq!(content.bytes.as_deref(), Some(&b"pkg/BUILD"[..]));

    let listing = session.evaluate(&DirListingKey(RootedPath::new(Root::Workspace, path("pkg/sub")))).await.unwrap();
    assert_eq!(listing.get(Symbol::intern("deep")), Some(EntryKind::Directory));
    assert_eq!(listing.get(Symbol::intern("c.cc")), Some(EntryKind::File));
}

#[tokio::test]
async fn globs_match_files_of_the_package() {
    let dir = tree();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());

    assert_eq!(matches(&graph, glob(&["*.cc"], &[], true)).await, [".hidden.cc", "a.cc", "b.cc"]);
    assert_eq!(
        matches(&graph, glob(&["**/*.cc"], &[], true)).await,
        [".hidden.cc", "a.cc", "b.cc", "sub/c.cc", "sub/deep/d.cc"]
    );
    assert_eq!(matches(&graph, glob(&["**/*.cc"], &["sub/**", ".*"], true)).await, ["a.cc", "b.cc"]);
    assert_eq!(matches(&graph, glob(&["sub/*"], &[], true)).await, ["sub/c.cc"]);
    assert_eq!(matches(&graph, glob(&["sub/*"], &[], false)).await, ["sub/c.cc", "sub/deep"]);
    assert_eq!(matches(&graph, glob(&["subpkg/*"], &[], true)).await, Vec::<String>::new());
}

#[tokio::test]
async fn globs_report_empty_includes() {
    let dir = tree();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    let result = graph.session().await.evaluate(&glob(&["*.cc", "*.java"], &[], true)).await.unwrap();
    assert_eq!(result.empty_includes, [1]);
}

#[tokio::test]
async fn globs_update_when_listings_change() {
    let dir = tree();
    let engine = Engine::default();
    let graph = graph(&engine, dir.path());
    assert_eq!(matches(&graph, glob(&["*.cc"], &[".*"], true)).await, ["a.cc", "b.cc"]);

    std::fs::write(dir.path().join("pkg/z.cc"), "z").unwrap();
    graph.invalidate(&[DirListingKey(RootedPath::new(Root::Workspace, path("pkg")))]).await;
    assert_eq!(matches(&graph, glob(&["*.cc"], &[".*"], true)).await, ["a.cc", "b.cc", "z.cc"]);
}

#[test]
fn patterns_match_paths() {
    let pattern = GlobPattern::parse("a/**/b*.c?").unwrap();
    assert!(pattern.matches(path("a/b.cc")));
    assert!(pattern.matches(path("a/x/y/bz.ch")));
    assert!(!pattern.matches(path("a/x/c.cc")));
    for bad in ["", "/a", "a//b", "a/../b", "a**"] {
        assert!(GlobPattern::parse(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn changes_are_found_by_statting_again() {
    let dir = tree();
    let engine = Engine::default();
    let changes = FileChanges::new();
    let graph = engine.graph(Globals::new().with(workspace(dir.path())).with(changes.clone()));
    let build = RootedPath::new(Root::Workspace, path("pkg/BUILD"));
    let listing = RootedPath::new(Root::Workspace, path("pkg"));
    {
        let session = graph.session().await;
        session.evaluate(&FileContentKey(build)).await.unwrap();
        session.evaluate(&DirListingKey(listing)).await.unwrap();
    }
    assert_eq!(changes.len(), 2);
    assert!(changes.changed().await.is_empty());

    std::fs::write(dir.path().join("pkg/BUILD"), "changed and longer").unwrap();
    std::fs::write(dir.path().join("pkg/new.cc"), "").unwrap();
    let mut changed = changes.changed().await;
    changed.sort_by_key(|p| p.path.to_string());
    assert_eq!(changed, [listing, build]);
}
