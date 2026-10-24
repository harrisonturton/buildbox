//! Applying unified diffs, as `patch -p<strip>` and `git apply` do.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("{patch}: {message}")]
    Invalid { patch: String, message: String },

    #[error("{patch}: hunk {hunk} of {file} doesn't apply")]
    Conflict { patch: String, file: String, hunk: usize },

    #[error("{context}: {error}")]
    Io { context: String, error: std::io::Error },
}

/// The changes to one file.
#[derive(Debug, Default)]
struct FileChange {
    old_path: Option<String>,
    new_path: Option<String>,
    hunks: Vec<Hunk>,
    new_mode: Option<u32>,
}

#[derive(Debug, Default)]
struct Hunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Apply the unified diff in `patch` to the tree at `dir`, removing `strip`
/// leading components from each path. Supports git's extended headers for
/// new, deleted and renamed files and mode changes. Blocks.
pub fn apply_patch(dir: &Path, patch: &str, strip: usize, name: &str) -> Result<(), PatchError> {
    let changes = parse(patch, name)?;
    for change in changes {
        apply_change(dir, &change, strip, name)?;
    }
    Ok(())
}

fn parse(patch: &str, name: &str) -> Result<Vec<FileChange>, PatchError> {
    let invalid = |message: String| PatchError::Invalid {
        patch: name.to_string(),
        message,
    };
    let mut changes = Vec::new();
    let mut current: Option<FileChange> = None;
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            if let Some(change) = current.take() {
                changes.push(change);
            }
            let mut change = FileChange::default();
            if let Some((old, new)) = git_header_paths(line) {
                change.old_path = Some(old);
                change.new_path = Some(new);
            }
            current = Some(change);
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            let change = current.get_or_insert_with(FileChange::default);
            if !change.hunks.is_empty() {
                changes.push(current.take().expect("just checked"));
                current = Some(FileChange::default());
            }
            current.as_mut().expect("just set").old_path = header_path(path);
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            current.get_or_insert_with(FileChange::default).new_path = header_path(path);
            continue;
        }
        if let Some(mode) = line.strip_prefix("new file mode ").or_else(|| line.strip_prefix("new mode ")) {
            let change = current.get_or_insert_with(FileChange::default);
            change.new_mode = u32::from_str_radix(mode.trim(), 8).ok();
            if line.starts_with("new file mode") {
                change.old_path = None;
            }
            continue;
        }
        if line.starts_with("deleted file mode") {
            current.get_or_insert_with(FileChange::default).new_path = None;
            continue;
        }
        if let Some(path) = line.strip_prefix("rename from ") {
            current.get_or_insert_with(FileChange::default).old_path = Some(format!("a/{path}"));
            continue;
        }
        if let Some(path) = line.strip_prefix("rename to ") {
            current.get_or_insert_with(FileChange::default).new_path = Some(format!("b/{path}"));
            continue;
        }
        if line.starts_with("@@") {
            let hunk = parse_hunk(line, &mut lines).ok_or_else(|| invalid(format!("invalid hunk header '{line}'")))?;
            current.get_or_insert_with(FileChange::default).hunks.push(hunk);
        }
    }
    if let Some(change) = current {
        changes.push(change);
    }
    Ok(changes)
}

/// The paths in `diff --git a/x b/x`, when they contain no spaces.
fn git_header_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let parts: Vec<&str> = rest.split(' ').collect();
    if parts.len() != 2 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

fn header_path(text: &str) -> Option<String> {
    let path = text.split('\t').next().unwrap_or(text).trim();
    if path == "/dev/null" {
        return None;
    }
    Some(path.to_string())
}

fn parse_hunk<'a>(header: &str, lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>) -> Option<Hunk> {
    let ranges = header.strip_prefix("@@ ")?.split(" @@").next()?;
    let mut parts = ranges.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range(old)?;
    let (_, new_count) = parse_range(new)?;
    let mut hunk = Hunk {
        old_start,
        lines: Vec::new(),
    };
    let (mut old_seen, mut new_seen) = (0, 0);
    while old_seen < old_count || new_seen < new_count {
        let Some(line) = lines.peek().copied() else {
            break;
        };
        if line.starts_with("\\") {
            lines.next();
            continue;
        }
        let (kind, text) = match line.chars().next() {
            Some(' ') => ('c', &line[1..]),
            Some('-') => ('-', &line[1..]),
            Some('+') => ('+', &line[1..]),
            None => ('c', ""),
            Some(_) => break,
        };
        lines.next();
        match kind {
            'c' => {
                old_seen += 1;
                new_seen += 1;
                hunk.lines.push(HunkLine::Context(text.to_string()));
            }
            '-' => {
                old_seen += 1;
                hunk.lines.push(HunkLine::Remove(text.to_string()));
            }
            _ => {
                new_seen += 1;
                hunk.lines.push(HunkLine::Add(text.to_string()));
            }
        }
    }
    while lines.peek().is_some_and(|line| line.starts_with('\\')) {
        lines.next();
    }
    Some(hunk)
}

fn parse_range(range: &str) -> Option<(usize, usize)> {
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

fn strip_path(path: &str, strip: usize, patch: &str) -> Result<PathBuf, PatchError> {
    let stripped: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).skip(strip).collect();
    let relative = PathBuf::from(stripped.join("/"));
    let safe = !stripped.is_empty() && relative.components().all(|component| matches!(component, Component::Normal(_)));
    if !safe {
        return Err(PatchError::Invalid {
            patch: patch.to_string(),
            message: format!("invalid path '{path}' after stripping {strip} components"),
        });
    }
    Ok(relative)
}

fn apply_change(dir: &Path, change: &FileChange, strip: usize, name: &str) -> Result<(), PatchError> {
    let io = |context: String| move |error| PatchError::Io { context, error };
    let old = change.old_path.as_deref().map(|path| strip_path(path, strip, name)).transpose()?;
    let new = change.new_path.as_deref().map(|path| strip_path(path, strip, name)).transpose()?;

    let creates = change.hunks.iter().all(|hunk| hunk.lines.iter().all(|line| matches!(line, HunkLine::Add(_))));
    let original = match &old {
        Some(old) if creates && !dir.join(old).exists() => String::new(),
        Some(old) => std::fs::read_to_string(dir.join(old)).map_err(io(format!("{name}: failed to read {}", old.display())))?,
        None => String::new(),
    };
    let display = new.as_ref().or(old.as_ref()).map(|p| p.display().to_string()).unwrap_or_default();
    let patched = apply_hunks(&original, &change.hunks, name, &display)?;

    let Some(new) = new else {
        if let Some(old) = old {
            std::fs::remove_file(dir.join(&old)).map_err(io(format!("{name}: failed to delete {}", old.display())))?;
        }
        return Ok(());
    };
    let target = dir.join(&new);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(io(format!("{name}: failed to create {}", parent.display())))?;
    }
    if let Some(old) = &old {
        if old != &new {
            let _ = std::fs::remove_file(dir.join(old));
        }
    }
    std::fs::write(&target, patched).map_err(io(format!("{name}: failed to write {}", target.display())))?;
    if let Some(mode) = change.new_mode {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o777));
    }
    Ok(())
}

/// Apply hunks in order, each at its stated position or, if the file has
/// shifted, the nearest place its context matches.
fn apply_hunks(original: &str, hunks: &[Hunk], patch: &str, file: &str) -> Result<String, PatchError> {
    let ends_with_newline = original.is_empty() || original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let mut offset: isize = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let new: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(text) | HunkLine::Add(text) => Some(text.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect();
        let expected = (hunk.old_start.saturating_sub(1) as isize + offset).max(0) as usize;
        let expected = if old.is_empty() { hunk.old_start.min(lines.len()) } else { expected };
        let Some(at) = find(&lines, &old, expected) else {
            return Err(PatchError::Conflict {
                patch: patch.to_string(),
                file: file.to_string(),
                hunk: index + 1,
            });
        };
        lines.splice(at..at + old.len(), new.iter().cloned());
        offset += at as isize - expected as isize + new.len() as isize - old.len() as isize;
    }
    let mut result = lines.join("\n");
    if !lines.is_empty() && ends_with_newline {
        result.push('\n');
    }
    Ok(result)
}

/// The position nearest `expected` where `needle` matches `lines`.
fn find(lines: &[String], needle: &[&str], expected: usize) -> Option<usize> {
    let matches_at = |at: usize| at + needle.len() <= lines.len() && needle.iter().zip(&lines[at..]).all(|(a, b)| *a == b);
    if needle.is_empty() {
        return Some(expected.min(lines.len()));
    }
    for distance in 0..=lines.len() {
        if expected >= distance && matches_at(expected - distance) {
            return Some(expected - distance);
        }
        if matches_at(expected + distance) {
            return Some(expected + distance);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_modifications_creations_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(dir.path().join("gone.txt"), "bye\n").unwrap();
        let patch = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -2,2 +2,3 @@
 two
-three
+THREE
+three and a half
diff --git a/new.txt b/new.txt
new file mode 100755
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";
        apply_patch(dir.path(), patch, 1, "test.patch").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "one\ntwo\nTHREE\nthree and a half\nfour\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("new.txt")).unwrap(), "hello\nworld\n");
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[test]
    fn hunks_apply_to_shifted_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "added\nadded\none\ntwo\n").unwrap();
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n one\n-two\n+2\n";
        apply_patch(dir.path(), patch, 1, "test.patch").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "added\nadded\none\n2\n");
    }

    #[test]
    fn hunks_from_nothing_create_files() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "--- MODULE.bazel\n+++ MODULE.bazel\n@@ -0,0 +1,2 @@\n+module(\n+)\n";
        apply_patch(dir.path(), patch, 0, "p").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("MODULE.bazel")).unwrap(), "module(\n)\n");
    }

    #[test]
    fn conflicts_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-y\n+z\n";
        assert!(matches!(apply_patch(dir.path(), patch, 1, "p"), Err(PatchError::Conflict { .. })));
    }
}
