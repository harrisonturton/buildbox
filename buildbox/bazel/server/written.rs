//! What builds wrote to a workspace's output tree, so that a build writes
//! only the outputs that changed, as Bazel leaves unchanged ones alone.

use bazel_execution::OutputFile;
use skycodec::Fingerprint;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The files a workspace's builds wrote, by absolute path, with what they
/// wrote: an output's digest, or a runfiles tree's layout.
#[derive(Default)]
pub struct Written(Mutex<HashMap<PathBuf, Record>>);

#[derive(Clone, Copy, PartialEq, Eq)]
struct Record {
    content: Fingerprint,
    file: FileIdentity,
}

/// Identifies a file on disk, so that one changed or replaced since it was
/// written is written again.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified: i64,
    modified_nanos: i64,
}

impl FileIdentity {
    fn of(path: &Path) -> Option<FileIdentity> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }
}

impl Written {
    /// The outputs that aren't already in the tree as they were written.
    /// Blocks, on stats.
    pub fn changed_outputs(&self, exec_root: &Path, outputs: &[OutputFile]) -> Vec<OutputFile> {
        let written = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let unchanged = |output: &OutputFile| {
            let path = exec_root.join(output.path.to_string());
            let Some(record) = written.get(&path) else {
                return false;
            };
            record.content == output_content(output) && FileIdentity::of(&path) == Some(record.file)
        };
        outputs.iter().filter(|output| !unchanged(output)).copied().collect()
    }

    /// Record outputs written. Blocks, on stats.
    pub fn record_outputs(&self, exec_root: &Path, outputs: &[OutputFile]) {
        let mut written = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for output in outputs {
            let path = exec_root.join(output.path.to_string());
            match FileIdentity::of(&path) {
                Some(file) => written.insert(path, Record { content: output_content(output), file }),
                None => written.remove(&path),
            };
        }
    }

    /// Whether the runfiles tree of `executable` is already in place as
    /// the layout `fingerprint` identifies. Blocks.
    pub fn runfiles_unchanged(&self, executable: &Path, fingerprint: Fingerprint) -> bool {
        let manifest = manifest(executable);
        let written = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = written.get(&manifest) else {
            return false;
        };
        record.content == fingerprint && FileIdentity::of(&manifest) == Some(record.file)
    }

    /// Record a runfiles tree written. Blocks.
    pub fn record_runfiles(&self, executable: &Path, fingerprint: Fingerprint) {
        let manifest = manifest(executable);
        let mut written = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match FileIdentity::of(&manifest) {
            Some(file) => written.insert(manifest, Record { content: fingerprint, file }),
            None => written.remove(&manifest),
        };
    }
}

fn output_content(output: &OutputFile) -> Fingerprint {
    Fingerprint::of(format!("{:?} {} {}", output.digest, output.executable, output.is_directory).as_bytes())
}

/// The runfiles manifest, which is written last, so it stands for the tree.
fn manifest(executable: &Path) -> PathBuf {
    PathBuf::from(format!("{}.runfiles_manifest", executable.display()))
}
