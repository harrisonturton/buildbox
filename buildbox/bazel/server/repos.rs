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

/// The path of the builtins archive inside Bazel's server jar.
const BUILTINS_ZIP: &str = "com/google/devtools/build/lib/bazel/rules/builtins_bzl.zip";

/// Find Bazel's `@_builtins` files, extracting them from the installation
/// `bazel_tools` came from into `cache_dir` the first time.
pub fn find_builtins(bazel_tools: &Path, cache_dir: &Path) -> Option<PathBuf> {
    let install = bazel_tools.parent()?;
    let key = install.file_name()?.to_string_lossy().into_owned();
    let dest = cache_dir.join(&key);
    let builtins = dest.join("builtins_bzl");
    if builtins.join("exports.bzl").is_file() {
        return Some(builtins);
    }
    match extract_builtins(&install.join("A-server.jar"), &dest) {
        Ok(()) => Some(builtins),
        Err(error) => {
            tracing::warn!("failed to extract @_builtins from {}: {error}", install.display());
            None
        }
    }
}

fn extract_builtins(jar: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(jar).map_err(|error| error.to_string())?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|error| error.to_string())?;
    let mut entry = archive.by_name(BUILTINS_ZIP).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(dest).map_err(|error| error.to_string())?;
    let inner = dest.join("builtins_bzl.zip");
    let mut out = std::fs::File::create(&inner).map_err(|error| error.to_string())?;
    std::io::copy(&mut entry, &mut out).map_err(|error| error.to_string())?;
    bazel_fetch::extract(&inner, bazel_fetch::ArchiveType::Zip, dest, "", &Default::default()).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(&inner);
    Ok(())
}
