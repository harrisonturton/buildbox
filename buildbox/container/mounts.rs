//! Building a container's filesystem: its image's root, mounted read-only,
//! with fresh `/proc`, `/dev` and `/tmp`, and host directories mounted in.
//!
//! Runs in the container's first process, in new user and mount
//! namespaces, so none of these mounts are visible outside it.

use crate::sys;
use std::io::ErrorKind;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// Devices bind-mounted from the host into the container's `/dev`.
const DEVICES: [&str; 6] = ["null", "zero", "full", "random", "urandom", "tty"];

/// Symlinks in the container's `/dev`, to where they point.
const DEV_SYMLINKS: [(&str, &str); 4] = [
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
];

/// Build the container's filesystem from the image at `rootfs`, with each
/// (host directory, container path) pair in `mounts` mounted in, and make it
/// the root.
pub fn set_up(rootfs: &Path, mounts: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    // Keep mounts made here from propagating back to the host.
    sys::mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)
        .map_err(|error| format!("failed to make mounts private: {error}"))?;

    // Mount points must exist before the image is made read-only.
    let mut mount_points = vec![PathBuf::from("proc"), PathBuf::from("dev"), PathBuf::from("tmp")];
    for (_, target) in mounts {
        mount_points.push(container_relative(target)?);
    }
    for mount_point in &mount_points {
        create_mount_point(rootfs, mount_point)?;
    }

    mount_root_read_only(rootfs)?;
    mount_filesystem(rootfs, "proc", "proc", libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, None)?;
    mount_filesystem(rootfs, "tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV, Some("mode=1777"))?;
    set_up_dev(&rootfs.join("dev"))?;
    for (source, target) in mounts {
        let target = rootfs.join(container_relative(target)?);
        bind_mount(source, &target)?;
    }

    std::env::set_current_dir(rootfs).map_err(|error| format!("failed to enter {}: {error}", rootfs.display()))?;
    sys::pivot_root_to_current_dir().map_err(|error| format!("failed to switch to the image's root: {error}"))
}

/// Mount the image on itself, read-only.
fn mount_root_read_only(rootfs: &Path) -> Result<(), String> {
    bind_mount(rootfs, rootfs)?;

    // Remounting in a user namespace has to keep the flags of the mount the
    // image is on, or it fails.
    let locked = sys::locked_mount_flags(rootfs)
        .map_err(|error| format!("failed to read the mount flags of {}: {error}", rootfs.display()))?;
    let flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | locked;
    sys::mount(None, rootfs, None, flags, None)
        .map_err(|error| format!("failed to make {} read-only: {error}", rootfs.display()))
}

/// A tmpfs with the host's basic devices.
fn set_up_dev(dev: &Path) -> Result<(), String> {
    let tmpfs = Some(Path::new("tmpfs"));
    sys::mount(tmpfs, dev, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NOEXEC, Some("mode=755"))
        .map_err(|error| format!("failed to mount /dev: {error}"))?;

    for device in DEVICES {
        let target = dev.join(device);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&target)
            .map_err(|error| format!("failed to create {}: {error}", target.display()))?;
        bind_mount(&Path::new("/dev").join(device), &target)?;
    }

    for (name, target) in DEV_SYMLINKS {
        let path = dev.join(name);
        std::os::unix::fs::symlink(target, &path).map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    }

    let shm = dev.join("shm");
    std::fs::create_dir(&shm).map_err(|error| format!("failed to create {}: {error}", shm.display()))?;
    sys::mount(tmpfs, &shm, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV, Some("mode=1777"))
        .map_err(|error| format!("failed to mount /dev/shm: {error}"))
}

fn mount_filesystem(
    rootfs: &Path,
    mount_point: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<(), String> {
    let target = rootfs.join(mount_point);
    sys::mount(Some(Path::new(fstype)), &target, Some(fstype), flags, data).map_err(|error| format!("failed to mount /{mount_point}: {error}"))
}

fn bind_mount(source: &Path, target: &Path) -> Result<(), String> {
    sys::mount(Some(source), target, None, libc::MS_BIND | libc::MS_REC, None)
        .map_err(|error| format!("failed to mount {} on {}: {error}", source.display(), target.display()))
}

/// Make sure the directory `path`, relative to `rootfs`, exists. Fails if
/// any part of it is a symlink or file in the image, so a mount can't be
/// redirected outside the container's root.
fn create_mount_point(rootfs: &Path, path: &Path) -> Result<(), String> {
    let mut dir = rootfs.to_path_buf();
    for component in path.components() {
        dir.push(component);
        match std::fs::symlink_metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {
                let message = format!("the image has /{} where a directory is needed", path.display());
                return Err(message);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("failed to stat {}: {error}", dir.display())),
        }

        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            // Another container made it first.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("failed to create {}: {error}", dir.display())),
        }
        let _ = std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o755));
    }
    Ok(())
}

/// An absolute path in the container, relative to its root.
fn container_relative(path: &Path) -> Result<PathBuf, String> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => relative.push(part),
            _ => return Err(format!("invalid mount path {}", path.display())),
        }
    }
    if !path.is_absolute() || relative.as_os_str().is_empty() {
        return Err(format!("invalid mount path {}", path.display()));
    }
    Ok(relative)
}
