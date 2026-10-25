//! The servers running on this machine, for this user: each server records
//! itself in a directory every client can find, whatever its config, and
//! removes its entry when it stops.
//!
//! An entry outlives a server that crashes, so readers check that its
//! process is still the one that wrote it, by pid and start time.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// What a running server records about itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pid: u32,
    /// When the process started, in clock ticks after boot, as
    /// `/proc/<pid>/stat` gives it, which tells the process apart from a
    /// later one with the same pid.
    pub start_time: u64,
    pub address: SocketAddr,
    /// The server's name, or `None` for the default server of its config.
    pub name: Option<String>,
    /// Where it keeps the state of its Bazel-compatible commands.
    pub state_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("couldn't read when process {0} started")]
    StartTime(u32),

    #[error("couldn't record the server in {path}: {error}")]
    Write { path: PathBuf, error: std::io::Error },
}

/// The directory entries go in: `bb/servers` in the user's runtime
/// directory, `$XDG_RUNTIME_DIR` or `/run/user/<uid>`, or else in the
/// temporary directory, under a name of the user's.
pub fn default_dir() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(runtime).join("bb/servers");
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let run_user = PathBuf::from(format!("/run/user/{uid}"));
    if run_user.is_dir() {
        return run_user.join("bb/servers");
    }
    std::env::temp_dir().join(format!("bb-{uid}/servers"))
}

/// A server's entry in a registry, removed when this is dropped.
#[derive(Debug)]
pub struct Registration {
    path: PathBuf,
    entry: Entry,
}

impl Registration {
    pub fn entry(&self) -> &Entry {
        &self.entry
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if read_entry(&self.path).is_some_and(|recorded| recorded == self.entry) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Record this process, a server listening at `address`, in the registry
/// at `dir`.
pub fn register(dir: &Path, address: SocketAddr, name: Option<String>, state_dir: PathBuf) -> Result<Registration, RegistryError> {
    let pid = std::process::id();
    let start_time = start_time(pid).ok_or(RegistryError::StartTime(pid))?;
    let entry = Entry {
        pid,
        start_time,
        address,
        name,
        state_dir,
    };
    let path = dir.join(format!("{pid}.json"));
    write_entry(dir, &path, &entry).map_err(|error| RegistryError::Write { path: dir.to_path_buf(), error })?;
    Ok(Registration { path, entry })
}

/// Write `entry` at `path` whole, so that a reader never sees half of it.
/// The directory is only the user's.
fn write_entry(dir: &Path, path: &Path, entry: &Entry) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let contents = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, path)
}

fn read_entry(path: &Path) -> Option<Entry> {
    let contents = std::fs::read(path).ok()?;
    serde_json::from_slice(&contents).ok()
}

/// The servers in the registry at `dir` whose processes are still running,
/// by pid. Entries of servers that have gone are removed.
pub fn running(dir: &Path) -> Vec<Entry> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for file in listing.flatten() {
        let path = file.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        match read_entry(&path) {
            Some(entry) if is_alive(&entry) => entries.push(entry),
            Some(_) => {
                let _ = std::fs::remove_file(&path);
            }
            None => {}
        }
    }
    entries.sort_by_key(|entry| entry.pid);
    entries
}

/// Whether the process that recorded `entry` is still running.
fn is_alive(entry: &Entry) -> bool {
    start_time(entry.pid) == Some(entry.start_time)
}

/// When process `pid` started, if it's running: field 22 of
/// `/proc/<pid>/stat`, counting from after its parenthesised name, which
/// may hold spaces.
pub fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_name) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    if matches!(fields.first(), Some(&("Z" | "X"))) {
        return None;
    }
    fields.get(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_listed_while_registered() {
        let dir = tempfile::tempdir().unwrap();
        let address: SocketAddr = "[::1]:50051".parse().unwrap();
        let registration = register(dir.path(), address, Some("agent".to_string()), PathBuf::from("/state")).unwrap();
        let listed = running(dir.path());
        assert_eq!(listed, [registration.entry().clone()]);
        assert_eq!(listed[0].pid, std::process::id());
        drop(registration);
        assert!(running(dir.path()).is_empty());
    }

    #[test]
    fn entries_of_gone_processes_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let entry = Entry {
            pid: std::process::id(),
            start_time: 0,
            address: "127.0.0.1:1".parse().unwrap(),
            name: None,
            state_dir: PathBuf::from("/state"),
        };
        let path = dir.path().join("1.json");
        write_entry(dir.path(), &path, &entry).unwrap();
        assert!(running(dir.path()).is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn this_process_has_a_start_time() {
        assert!(start_time(std::process::id()).is_some());
    }
}
