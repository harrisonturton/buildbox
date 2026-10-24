//! Unpacking container image layers into a root filesystem.
//!
//! Layers come from clients, so nothing in one is trusted: paths can't
//! escape the root, and nothing is created through a symlink. Files are
//! owned by the user buildbox runs as; ownership in the layer is ignored,
//! and so are set-user-ID bits and device files.

use artifacts::ArtifactError;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use tar::{Entry, EntryType};

/// Files with this prefix mark a file from a lower layer as deleted.
const WHITEOUT_PREFIX: &str = ".wh.";

/// This file marks its directory as replacing, not merging with, the same
/// directory in lower layers.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Directories are always readable, writable and searchable by their owner,
/// so later layers can change them and the image can be deleted.
const DIR_OWNER_MODE: u32 = 0o700;

/// Apply a layer, given as an uncompressed tar stream, on top of the files
/// already in `root`. Returns the total size of the files it wrote.
pub fn apply_layer(layer: impl Read, root: &Path) -> Result<u64, ArtifactError> {
    let mut archive = tar::Archive::new(layer);
    let entries = archive
        .entries()
        .map_err(|error| ArtifactError::io("failed to read layer", error))?;

    let mut layer = Layer {
        root,
        created: HashSet::new(),
        bytes: 0,
    };
    for entry in entries {
        let entry = entry.map_err(|error| ArtifactError::io("failed to read layer", error))?;
        layer.apply_entry(entry)?;
    }
    Ok(layer.bytes)
}

/// The state of applying one layer.
struct Layer<'a> {
    root: &'a Path,

    /// Paths this layer has created, which opaque whiteouts must keep.
    created: HashSet<PathBuf>,

    bytes: u64,
}

impl Layer<'_> {
    fn apply_entry<R: Read>(&mut self, mut entry: Entry<'_, R>) -> Result<(), ArtifactError> {
        let raw_path = entry.path_bytes().into_owned();
        // The root directory itself, which exists already.
        let Some(path) = parse_entry_path(&raw_path)? else {
            return Ok(());
        };

        // A path parsed from an entry always has a final component.
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if let Some(hidden) = name.strip_prefix(WHITEOUT_PREFIX) {
            let hidden = hidden.to_string();
            return self.apply_whiteout(&path, &hidden);
        }

        let entry_type = entry.header().entry_type();
        let mode = entry
            .header()
            .mode()
            .map_err(|error| invalid(format!("{} has an invalid mode: {error}", path.display())))?;
        let target = self.root.join(&path);
        create_parent_dirs(self.root, &path)?;

        match entry_type {
            EntryType::Directory => create_dir(&target, mode)?,
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse => {
                self.bytes += write_file(&mut entry, &target, mode)?;
            }
            EntryType::Symlink => create_symlink(&entry, &target)?,
            EntryType::Link => create_hard_link(self.root, &entry, &target)?,
            // Devices can't be created without privileges, and FIFOs aren't
            // useful in a build. Other types carry metadata.
            _ => {
                tracing::debug!("skipping {} of type {entry_type:?}", path.display());
                return Ok(());
            }
        }
        self.record_created(&path);
        Ok(())
    }

    fn apply_whiteout(&self, path: &Path, hidden: &str) -> Result<(), ArtifactError> {
        let parent = path.parent().unwrap_or(Path::new(""));
        if !existing_dir_is_safe(self.root, parent)? {
            // Nothing below a missing directory can need deleting.
            return Ok(());
        }

        let is_opaque = path.file_name() == Some(OsStr::new(OPAQUE_WHITEOUT));
        if is_opaque {
            return self.clear_dir_from_lower_layers(parent);
        }

        let hidden_path = parse_entry_path(hidden.as_bytes())?;
        let is_single_name = hidden_path.as_ref().is_some_and(|hidden| hidden.components().count() == 1);
        if !is_single_name {
            return Err(invalid(format!("invalid whiteout {}", path.display())));
        }
        remove_if_exists(&self.root.join(parent).join(hidden))
    }

    /// Delete everything in `dir` that this layer didn't create.
    fn clear_dir_from_lower_layers(&self, dir: &Path) -> Result<(), ArtifactError> {
        let host_dir = self.root.join(dir);
        let entries = std::fs::read_dir(&host_dir).map_err(|error| io_error("failed to list", &host_dir, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| io_error("failed to list", &host_dir, error))?;
            let path = dir.join(entry.file_name());
            if !self.created.contains(&path) {
                remove_if_exists(&self.root.join(&path))?;
            }
        }
        Ok(())
    }

    /// Record that `path`, and so every directory above it, is part of this
    /// layer.
    fn record_created(&mut self, path: &Path) {
        for ancestor in path.ancestors() {
            if ancestor.as_os_str().is_empty() || !self.created.insert(ancestor.to_path_buf()) {
                break;
            }
        }
    }
}

/// Parse a path from a layer into a relative path with only normal
/// components. Returns `None` for the root directory. Leading slashes are
/// ignored, as other tools do, but `..` is rejected.
fn parse_entry_path(raw: &[u8]) -> Result<Option<PathBuf>, ArtifactError> {
    let raw_path = Path::new(OsStr::from_bytes(raw));
    let mut path = PathBuf::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid(format!("path {} escapes the image root", raw_path.display())));
            }
        }
    }
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(path))
}

/// Create the directories above `path` that don't exist. Fails if one of
/// them is a symlink, which could lead outside the root. A file where a
/// directory should be, left by a lower layer, is replaced.
fn create_parent_dirs(root: &Path, path: &Path) -> Result<(), ArtifactError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    let mut dir = root.to_path_buf();
    for component in parent.components() {
        dir.push(component);
        let metadata = match std::fs::symlink_metadata(&dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                create_dir(&dir, 0o755)?;
                continue;
            }
            Err(error) => return Err(io_error("failed to stat", &dir, error)),
        };

        if metadata.is_dir() {
            continue;
        }
        if metadata.is_symlink() {
            return Err(invalid(format!("{} is created through a symlink", path.display())));
        }
        remove_if_exists(&dir)?;
        create_dir(&dir, 0o755)?;
    }
    Ok(())
}

/// Whether `dir` exists as a real directory, reached without following any
/// symlinks.
fn existing_dir_is_safe(root: &Path, dir: &Path) -> Result<bool, ArtifactError> {
    let mut path = root.to_path_buf();
    for component in dir.components() {
        path.push(component);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("failed to stat", &path, error)),
        };
        if !metadata.is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Create a directory, or update the mode of one that exists. Anything
/// else in the way is replaced.
fn create_dir(path: &Path, mode: u32) -> Result<(), ArtifactError> {
    let existing = std::fs::symlink_metadata(path);
    let is_dir = existing.as_ref().is_ok_and(|metadata| metadata.is_dir());
    if !is_dir {
        remove_if_exists(path)?;
        std::fs::create_dir(path).map_err(|error| io_error("failed to create", path, error))?;
    }

    let mode = (mode & 0o777) | DIR_OWNER_MODE;
    std::fs::set_permissions(path, PermissionsExt::from_mode(mode))
        .map_err(|error| io_error("failed to set permissions on", path, error))
}

/// Write a regular file, replacing whatever is there. Returns its size.
fn write_file(contents: &mut impl Read, path: &Path, mode: u32) -> Result<u64, ArtifactError> {
    remove_if_exists(path)?;

    // Created new, so writing can't follow a symlink planted at `path`.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| io_error("failed to create", path, error))?;
    let size = std::io::copy(contents, &mut file).map_err(|error| io_error("failed to write", path, error))?;

    file.set_permissions(PermissionsExt::from_mode(mode & 0o777))
        .map_err(|error| io_error("failed to set permissions on", path, error))?;
    Ok(size)
}

/// Create a symlink. Its target isn't checked: it is only ever followed
/// inside a container, where the image root is the root.
fn create_symlink<R: Read>(entry: &Entry<'_, R>, path: &Path) -> Result<(), ArtifactError> {
    let target = link_name(entry, path)?;
    remove_if_exists(path)?;
    std::os::unix::fs::symlink(OsStr::from_bytes(&target), path)
        .map_err(|error| io_error("failed to create symlink", path, error))
}

/// Create a hard link to a file already unpacked.
fn create_hard_link<R: Read>(root: &Path, entry: &Entry<'_, R>, path: &Path) -> Result<(), ArtifactError> {
    let target = link_name(entry, path)?;
    let Some(target) = parse_entry_path(&target)? else {
        return Err(invalid(format!("hard link {} points at the image root", path.display())));
    };

    let target_parent = target.parent().unwrap_or(Path::new(""));
    let source = root.join(&target);
    let source_is_file = std::fs::symlink_metadata(&source).is_ok_and(|metadata| !metadata.is_dir());
    if !existing_dir_is_safe(root, target_parent)? || !source_is_file {
        return Err(invalid(format!("hard link {} points at a missing file", path.display())));
    }

    remove_if_exists(path)?;
    std::fs::hard_link(&source, path).map_err(|error| io_error("failed to create hard link", path, error))
}

fn link_name<R: Read>(entry: &Entry<'_, R>, path: &Path) -> Result<Vec<u8>, ArtifactError> {
    let target = entry
        .link_name_bytes()
        .map(|target| target.into_owned())
        .unwrap_or_default();
    if target.is_empty() {
        return Err(invalid(format!("link {} has no target", path.display())));
    }
    Ok(target)
}

/// Remove a file, symlink or directory tree, if there is one.
fn remove_if_exists(path: &Path) -> Result<(), ArtifactError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("failed to stat", path, error)),
    };

    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    removed.map_err(|error| io_error("failed to remove", path, error))
}

fn invalid(message: String) -> ArtifactError {
    ArtifactError::InvalidImage(message)
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ArtifactError {
    ArtifactError::io(format!("{action} {}", path.display()), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a layer in memory.
    struct LayerBuilder {
        builder: tar::Builder<Vec<u8>>,
    }

    impl LayerBuilder {
        fn new() -> LayerBuilder {
            LayerBuilder {
                builder: tar::Builder::new(vec![]),
            }
        }

        fn header(path: &str, entry_type: EntryType, mode: u32, size: u64) -> tar::Header {
            let mut header = tar::Header::new_gnu();
            // Set the raw name, so tests can write paths tar::Builder rejects.
            let name = &mut header.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_entry_type(entry_type);
            header.set_mode(mode);
            header.set_size(size);
            header.set_cksum();
            header
        }

        fn file(mut self, path: &str, contents: &[u8], mode: u32) -> Self {
            let header = Self::header(path, EntryType::Regular, mode, contents.len() as u64);
            self.builder.append(&header, contents).unwrap();
            self
        }

        fn dir(mut self, path: &str, mode: u32) -> Self {
            let header = Self::header(path, EntryType::Directory, mode, 0);
            self.builder.append(&header, &[][..]).unwrap();
            self
        }

        fn link(mut self, path: &str, entry_type: EntryType, target: &str) -> Self {
            let mut header = Self::header(path, entry_type, 0o777, 0);
            header.set_link_name(target).unwrap();
            header.set_cksum();
            self.builder.append(&header, &[][..]).unwrap();
            self
        }

        fn build(self) -> Vec<u8> {
            self.builder.into_inner().unwrap()
        }
    }

    fn apply(root: &Path, layer: Vec<u8>) -> Result<u64, ArtifactError> {
        apply_layer(layer.as_slice(), root)
    }

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn unpacks_files_directories_and_links() {
        let root = tempfile::tempdir().unwrap();
        let layer = LayerBuilder::new()
            .dir("usr/", 0o755)
            .dir("usr/bin/", 0o555)
            .file("usr/bin/tool", b"#!/bin/sh\n", 0o4755)
            .link("bin", EntryType::Symlink, "usr/bin")
            .link("usr/bin/alias", EntryType::Link, "usr/bin/tool")
            .file("./etc/conf", b"x", 0o640)
            .build();

        let bytes = apply(root.path(), layer).unwrap();

        assert_eq!(bytes, 11);
        let tool = root.path().join("usr/bin/tool");
        assert_eq!(std::fs::read(&tool).unwrap(), b"#!/bin/sh\n");
        assert_eq!(mode(&tool), 0o755, "set-user-ID bits are dropped");
        assert_eq!(mode(&root.path().join("usr/bin")), 0o755, "directories stay writable by their owner");
        assert_eq!(std::fs::read_link(root.path().join("bin")).unwrap(), Path::new("usr/bin"));
        assert_eq!(std::fs::read(root.path().join("usr/bin/alias")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(mode(&root.path().join("etc/conf")), 0o640);
    }

    #[test]
    fn later_layers_replace_and_delete_files() {
        let root = tempfile::tempdir().unwrap();
        let lower = LayerBuilder::new()
            .file("a/keep", b"old", 0o644)
            .file("a/replace", b"old", 0o444)
            .file("a/delete", b"old", 0o644)
            .file("b", b"file becomes a directory", 0o644)
            .build();
        let upper = LayerBuilder::new()
            .file("a/replace", b"new", 0o644)
            .file("a/.wh.delete", b"", 0o644)
            .file("b/inside", b"new", 0o644)
            .build();

        apply(root.path(), lower).unwrap();
        apply(root.path(), upper).unwrap();

        assert_eq!(std::fs::read(root.path().join("a/keep")).unwrap(), b"old");
        assert_eq!(std::fs::read(root.path().join("a/replace")).unwrap(), b"new");
        assert!(!root.path().join("a/delete").exists());
        assert!(!root.path().join("a/.wh.delete").exists());
        assert_eq!(std::fs::read(root.path().join("b/inside")).unwrap(), b"new");
    }

    #[test]
    fn opaque_whiteouts_hide_only_lower_layers() {
        let root = tempfile::tempdir().unwrap();
        let lower = LayerBuilder::new()
            .file("dir/old", b"old", 0o644)
            .file("dir/sub/old", b"old", 0o644)
            .build();
        let upper = LayerBuilder::new()
            .dir("dir/", 0o755)
            .file("dir/new", b"new", 0o644)
            .file("dir/.wh..wh..opq", b"", 0o644)
            .build();

        apply(root.path(), lower).unwrap();
        apply(root.path(), upper).unwrap();

        let mut names: Vec<String> = std::fs::read_dir(root.path().join("dir"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["new"]);
    }

    #[test]
    fn rejects_paths_that_escape_the_root() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let layer = LayerBuilder::new().file("../escaped", b"x", 0o644).build();

        let err = apply(&root, layer).unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
        assert!(!parent.path().join("escaped").exists());
    }

    #[test]
    fn never_writes_through_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let layer = LayerBuilder::new()
            .link("escape", EntryType::Symlink, outside.to_str().unwrap())
            .file("escape/file", b"x", 0o644)
            .build();

        let err = apply(&root, layer).unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn replaces_symlinks_rather_than_writing_through_them() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, b"untouched").unwrap();
        let layer = LayerBuilder::new()
            .link("file", EntryType::Symlink, outside.to_str().unwrap())
            .file("file", b"replaced", 0o644)
            .build();

        apply(&root, layer).unwrap();

        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"replaced");
    }

    #[test]
    fn rejects_hard_links_to_missing_files() {
        let root = tempfile::tempdir().unwrap();
        let layer = LayerBuilder::new().link("link", EntryType::Link, "../../etc/passwd").build();

        let err = apply(root.path(), layer).unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
    }

    #[test]
    fn rejects_whiteouts_that_name_paths() {
        let root = tempfile::tempdir().unwrap();
        let layer = LayerBuilder::new().file("dir/.wh...", b"", 0o644).build();
        std::fs::create_dir(root.path().join("dir")).unwrap();

        let err = apply(root.path(), layer).unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
        assert!(root.path().exists());
    }
}
