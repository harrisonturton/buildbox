//! The runfiles trees of the executables a build builds.

use bazel_analysis::{Artifact, ConfiguredTarget, FileValue, Runfiles, runfiles_repo_name};
use bazel_execution::RunfilesLayout;
use bazel_fs::Root;
use bazel_label::RepoName;
use bazel_loading::RepoRootKey;
use skyframe::{SkyError, Session};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The runfiles tree of a target's executable, if it has one.
pub async fn layout(session: &Session, target: &ConfiguredTarget, exec_root: &Path, workspace: &Path) -> Result<Option<RunfilesLayout>, SkyError> {
    let Some((executable, entries)) = executable_runfiles(target) else {
        return Ok(None);
    };
    let mut roots = HashMap::new();
    let mut linked = Vec::with_capacity(entries.len());
    for (path, artifact) in &entries {
        linked.push((path.clone(), location(session, &mut roots, artifact, exec_root, workspace).await?));
    }
    Ok(Some(RunfilesLayout {
        executable: exec_root.join(executable.exec_path.to_string()),
        entries: linked,
        repo_mapping: bazel_execution::repo_mapping(session, &entries).await?,
    }))
}

/// A target's executable, if it has one.
pub fn executable(target: &ConfiguredTarget) -> Option<Artifact> {
    FileValue::artifact(target.target_value()?.default_info("executable")?)
}

/// A target's executable, and its runfiles.
pub fn executable_runfiles(target: &ConfiguredTarget) -> Option<(Artifact, Vec<(String, Artifact)>)> {
    let value = target.target_value()?;
    let executable = FileValue::artifact(value.default_info("executable")?)?;
    let runfiles = Runfiles::from_value(value.default_info("default_runfiles")?)?;
    Some((executable, runfiles.entries(runfiles_repo_name(RepoName::MAIN))))
}

/// Where an artifact is: in the output tree, or in its repository's source
/// tree. `roots` keeps the repositories' directories found so far, since a
/// tree has many files from few repositories.
async fn location(
    session: &Session,
    roots: &mut HashMap<RepoName, PathBuf>,
    artifact: &Artifact,
    exec_root: &Path,
    workspace: &Path,
) -> Result<PathBuf, SkyError> {
    if !artifact.is_source() {
        return Ok(exec_root.join(artifact.exec_path.to_string()));
    }
    let label = artifact.owner;
    let relative = label.package.path.join_path(bazel_analysis::name_path(label)).to_string();
    let repo = label.repo();
    if let Some(base) = roots.get(&repo) {
        return Ok(base.join(relative));
    }
    let root = session.evaluate(&RepoRootKey(repo)).await?;
    let base = match root.0 {
        Root::Workspace => workspace.to_path_buf(),
        Root::Dir(dir) => PathBuf::from(dir.as_str()),
    };
    let location = base.join(relative);
    roots.insert(repo, base);
    Ok(location)
}
