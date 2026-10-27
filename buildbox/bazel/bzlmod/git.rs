//! Checking out Git repositories, as Bazel's `git_repository` does: a
//! shallow fetch of the commit, tag or branch, falling back to a full
//! fetch, with submodules if asked for. Blocks.

use crate::module_file::GitSource;
use std::path::Path;
use std::process::Command;

/// Variables that would point `git` at a repository other than the one
/// being checked out, which Bazel clears.
const LOCAL_ENV_VARS: [&str; 16] = [
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

/// Where a checkout starts from: what to fetch, what to reset to, and how
/// shallow to fetch.
struct Refs {
    fetch: String,
    reset: String,
    shallow: String,
}

/// Check `source` out into `dir`, named `repo` in errors, leaving its
/// `.git` directory.
pub fn checkout(dir: &Path, repo: &str, source: &GitSource) -> Result<(), String> {
    let refs = refs(source)?;
    let git = Git { dir, repo };
    std::fs::create_dir_all(dir).map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    git.run(&["init", "."])?;
    git.run(&["remote", "add", "origin", &source.remote])?;
    fetch(&git, source, &refs)?;
    git.run(&["reset", "--hard", &refs.reset])?;
    git.run(&["clean", "-xdf"])?;
    let submodules: &[&str] = match (source.recursive_init_submodules, source.init_submodules) {
        (true, _) => &["-c", "protocol.file.allow=always", "submodule", "update", "--init", "--recursive", "--checkout", "--force"],
        (false, true) => &["-c", "protocol.file.allow=always", "submodule", "update", "--init", "--checkout", "--force"],
        (false, false) => &[],
    };
    if !submodules.is_empty() {
        git.run_maybe_shallow(&refs.shallow, submodules)?;
    }
    Ok(())
}

/// The refs to fetch and reset to, as `git_repository` picks them.
fn refs(source: &GitSource) -> Result<Refs, String> {
    let given = [&source.commit, &source.tag, &source.branch].iter().filter(|given| !given.is_empty()).count();
    if given > 1 {
        return Err("At most one of commit, tag, or branch may be provided".to_string());
    }
    if !source.shallow_since.is_empty() && !source.tag.is_empty() {
        return Err("shallow_since not allowed if a tag is specified; --depth=1 will be used for tags".to_string());
    }
    if !source.shallow_since.is_empty() && !source.branch.is_empty() {
        return Err("shallow_since not allowed if a branch is specified; --depth=1 will be used for branches".to_string());
    }
    let shallow = match source.shallow_since.is_empty() {
        true => "--depth=1".to_string(),
        false => format!("--shallow-since={}", source.shallow_since),
    };
    let (fetch, reset) = if !source.commit.is_empty() {
        (source.commit.clone(), source.commit.clone())
    } else if !source.tag.is_empty() {
        (format!("tags/{0}:tags/{0}", source.tag), format!("tags/{}", source.tag))
    } else if !source.branch.is_empty() {
        (format!("{0}:origin/{0}", source.branch), format!("origin/{}", source.branch))
    } else {
        ("HEAD:refs/remotes/origin/HEAD".to_string(), "origin/HEAD".to_string())
    };
    Ok(Refs { fetch, reset, shallow })
}

/// Fetch the ref, shallowly if the server allows, or for a commit, every
/// branch and tag if it can't fetch the commit alone.
fn fetch(git: &Git<'_>, source: &GitSource, refs: &Refs) -> Result<(), String> {
    let fetched = git.run_maybe_shallow(&refs.shallow, &["fetch", "origin", &refs.fetch]);
    if fetched.is_ok() || source.commit.is_empty() {
        return fetched;
    }
    git.run(&["fetch", "origin", "refs/heads/*:refs/remotes/origin/*", "refs/tags/*:refs/tags/*"])
}

/// Runs `git` in a checkout.
struct Git<'a> {
    dir: &'a Path,
    repo: &'a str,
}

impl Git<'_> {
    fn run(&self, args: &[&str]) -> Result<(), String> {
        let output = self.command(args).output().map_err(|error| self.error(args, &error.to_string()))?;
        match output.status.success() {
            true => Ok(()),
            false => Err(self.error(args, &String::from_utf8_lossy(&output.stderr))),
        }
    }

    /// Run a command with `shallow` after its first argument, and again
    /// without it if that fails.
    fn run_maybe_shallow(&self, shallow: &str, args: &[&str]) -> Result<(), String> {
        let mut with_shallow = vec![args[0], shallow];
        with_shallow.extend_from_slice(&args[1..]);
        match self.run(&with_shallow) {
            Ok(()) => Ok(()),
            Err(_) => self.run(args),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.args(["-c", "core.fsmonitor=false"]).args(args).current_dir(self.dir).stdin(std::process::Stdio::null());
        for name in LOCAL_ENV_VARS {
            command.env_remove(name);
        }
        command
    }

    /// The error for a failed command, as Bazel words it.
    fn error(&self, args: &[&str], stderr: &str) -> String {
        format!("error running 'git {}' while working with @{}:\n{stderr}", args.join(" "), self.repo)
    }
}
