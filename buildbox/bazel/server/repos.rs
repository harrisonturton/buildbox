//! Finding Bazel's built-in `@bazel_tools`.

use std::path::{Path, PathBuf};

/// Find `@bazel_tools`'s files: in `BB_BAZEL_TOOLS`, or the `embedded_tools`
/// of an installed Bazel, preferring `wanted_version`, and otherwise the
/// most recently installed.
pub fn find_bazel_tools(home: Option<&Path>, wanted_version: Option<&str>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BB_BAZEL_TOOLS") {
        return Some(PathBuf::from(dir));
    }
    let user = std::env::var("USER").ok();
    find_installed_tools(&install_dirs(home, user.as_deref()), wanted_version)
}

/// The directories Bazel installs itself in, one per installed version,
/// beneath its default output user roots: `~/.cache/bazel/_bazel_$USER` on
/// Linux, and on macOS `~/Library/Caches/bazel/_bazel_$USER`, or
/// `/private/var/tmp/_bazel_$USER` for older Bazels.
fn install_dirs(home: Option<&Path>, user: Option<&str>) -> Vec<PathBuf> {
    let mut output_user_roots = Vec::new();
    for cache in [".cache/bazel", "Library/Caches/bazel"] {
        let Some(home) = home else {
            break;
        };
        let entries = std::fs::read_dir(home.join(cache)).into_iter().flatten().flatten();
        output_user_roots.extend(entries.map(|entry| entry.path()));
    }
    if let Some(user) = user {
        output_user_roots.push(PathBuf::from(format!("/private/var/tmp/_bazel_{user}")));
    }
    output_user_roots.iter().map(|root| root.join("install")).collect()
}

/// The `embedded_tools` of the best Bazel installed in `install_dirs`:
/// version `wanted_version` if it's there, and otherwise the most recently
/// installed.
fn find_installed_tools(install_dirs: &[PathBuf], wanted_version: Option<&str>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for dir in install_dirs {
        let Ok(installs) = std::fs::read_dir(dir) else {
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
    bazel_fetch::extract(&inner, bazel_fetch::ArchiveType::Zip, dest, "", 0, &Default::default()).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(&inner);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install a fake Bazel `version` as `hash` in the `install` directory
    /// beneath `root`.
    fn install(root: &Path, hash: &str, version: &str) -> PathBuf {
        let dir = root.join("install").join(hash);
        std::fs::create_dir_all(dir.join("embedded_tools/tools")).unwrap();
        std::fs::write(dir.join("build-label.txt"), format!("{version}\n")).unwrap();
        dir.join("embedded_tools")
    }

    #[test]
    fn finds_bazel_installed_under_either_operating_systems_root() {
        let root = tempfile::tempdir().unwrap();
        let linux = root.path().join("home/.cache/bazel/_bazel_me");
        let macos = root.path().join("private/var/tmp/_bazel_me");
        install(&linux, "aaa", "8.7.0");
        let wanted = install(&macos, "bbb", "9.3.1");

        let dirs = [linux.join("install"), macos.join("install")];

        assert_eq!(find_installed_tools(&dirs, Some("9.3.1")), Some(wanted));
        assert!(find_installed_tools(&dirs[..1], Some("9.3.1")).is_some(), "another version will do");
        assert_eq!(find_installed_tools(&[root.path().join("missing")], None), None);
    }

    #[test]
    fn looks_where_linux_and_macos_install_bazel() {
        let home = tempfile::tempdir().unwrap();
        for root in [".cache/bazel/_bazel_me", "Library/Caches/bazel/_bazel_me"] {
            std::fs::create_dir_all(home.path().join(root)).unwrap();
        }

        let dirs = install_dirs(Some(home.path()), Some("me"));

        let expected = [
            home.path().join(".cache/bazel/_bazel_me/install"),
            home.path().join("Library/Caches/bazel/_bazel_me/install"),
            PathBuf::from("/private/var/tmp/_bazel_me/install"),
        ];
        assert_eq!(dirs, expected);
    }
}
