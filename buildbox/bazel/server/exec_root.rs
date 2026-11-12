//! Links in the execution root to the workspace's files and to external
//! repositories, as Bazel keeps them, so that the paths in actions' command
//! lines work outside a sandbox: an editor that reads `aquery
//! --output=compile_commands` runs the compiler in the execution root.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

/// The names of the external repositories that `text` mentions as
/// `external/<name>/`, as compile commands' paths do.
pub fn mentioned_repos(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut rest = text;
    while let Some(found) = rest.find("external/") {
        let after = &rest[found + "external/".len()..];
        let end = after.find(['/', '"', ' ', '\n']).unwrap_or(after.len());
        let name = &after[..end];
        let starts_path = found == 0 || matches!(rest.as_bytes()[found - 1], b'"' | b' ' | b'=' | b'I' | b'/' | b',');
        let ends_name = end == after.len() || matches!(after.as_bytes()[end], b'/' | b'"' | b' ' | b'\n');
        if starts_path && ends_name && !name.is_empty() {
            names.insert(name.to_string());
        }
        rest = &after[end..];
    }
    names
}

/// Link `exec_root/<entry>` to each top-level entry of `workspace_root`, and
/// `exec_root/external/<name>` to each repository's directory. A link that
/// points elsewhere is replaced; an entry that isn't a link, such as
/// `bazel-out`, is left alone.
pub fn plant(exec_root: &Path, workspace_root: &Path, repos: &[(String, PathBuf)]) -> io::Result<()> {
    std::fs::create_dir_all(exec_root.join("external"))?;
    for entry in std::fs::read_dir(workspace_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with("bazel-") || text == "external" {
            continue;
        }
        link(&exec_root.join(&name), &entry.path())?;
    }
    for (name, dir) in repos {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            continue;
        }
        link(&exec_root.join("external").join(name), dir)?;
    }
    Ok(())
}

/// Make `at` a link to `target`, unless something other than a link is
/// there.
fn link(at: &Path, target: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(at) {
        Ok(metadata) if !metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => {
            if std::fs::read_link(at)? == target {
                return Ok(());
            }
            std::fs::remove_file(at)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::os::unix::fs::symlink(target, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_repositories_named_in_paths() {
        let text = r#"{"arguments":["external/toolchains_llvm++llvm+llvm_toolchain/bin/cc_wrapper.sh","-iquote","external/zstd+","-Iexternal/llvm_zlib/include","--sysroot=external/sysroot+/","not-external/x/y"]}"#;
        let names: Vec<String> = mentioned_repos(text).into_iter().collect();
        assert_eq!(names, ["llvm_zlib", "sysroot+", "toolchains_llvm++llvm+llvm_toolchain", "zstd+"]);
    }

    #[test]
    fn links_the_workspace_and_repositories() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("MODULE.bazel"), "").unwrap();
        std::os::unix::fs::symlink("/elsewhere", workspace.path().join("bazel-out")).unwrap();
        let repo = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let exec_root = output.path().join("execroot/_main");
        std::fs::create_dir_all(exec_root.join("bazel-out")).unwrap();

        let repos = vec![("zstd+".to_string(), repo.path().to_path_buf())];
        plant(&exec_root, workspace.path(), &repos).unwrap();
        assert_eq!(std::fs::read_link(exec_root.join("src")).unwrap(), workspace.path().join("src"));
        assert_eq!(std::fs::read_link(exec_root.join("MODULE.bazel")).unwrap(), workspace.path().join("MODULE.bazel"));
        assert_eq!(std::fs::read_link(exec_root.join("external/zstd+")).unwrap(), repo.path());
        assert!(!std::fs::symlink_metadata(exec_root.join("bazel-out")).unwrap().file_type().is_symlink(), "bazel-out stays");

        // Planting again replaces a link that moved, and keeps the rest.
        let moved = tempfile::tempdir().unwrap();
        plant(&exec_root, workspace.path(), &[("zstd+".to_string(), moved.path().to_path_buf())]).unwrap();
        assert_eq!(std::fs::read_link(exec_root.join("external/zstd+")).unwrap(), moved.path());
    }
}
