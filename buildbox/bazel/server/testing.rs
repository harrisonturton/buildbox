//! Workspaces on disk for tests, with the services a server gives their
//! graphs.

use bazel_bzlmod::config::BzlmodConfig;
use bazel_bzlmod::resolver::BzlmodRepositories;
use bazel_loading::{BuiltinsDir, Repositories};
use skyframe::{Engine, Globals};
use std::path::Path;
use std::sync::Arc;

/// Write `contents` to `path`, making its directory.
pub fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Workspaces whose graphs fetch into `cache`, with a `@bazel_tools` of
/// their own there.
pub fn workspaces(cache: &Path) -> crate::workspace::Workspaces {
    let tools = cache.join("bazel_tools");
    write(&tools.join("MODULE.bazel"), "module(name = 'bazel_tools')\n");
    let config = BzlmodConfig::new(cache.to_path_buf(), cache.join("repository_cache"), Some(tools));
    let services = Globals::new()
        .with(Arc::new(Repositories(Arc::new(BzlmodRepositories))))
        .with(Arc::new(config))
        .with(Arc::new(BuiltinsDir(None)));
    crate::workspace::Workspaces::new(Engine::default(), services)
}
