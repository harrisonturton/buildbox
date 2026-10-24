//! Finding Bazel's built-in `@bazel_tools`.

use std::path::{Path, PathBuf};

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
