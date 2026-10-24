//! The runfiles trees of the executables a build builds.

use bazel_analysis::{Artifact, ConfiguredTarget, FileValue, Runfiles, runfiles_repo_name};
use bazel_execution::RunfilesLayout;
use bazel_fs::Root;
use bazel_label::RepoName;
use bazel_loading::{RepoMappingKey, RepoRootKey};
use skyframe::{SkyError, Session};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The runfiles tree of a target's executable, if it has one.
pub async fn layout(session: &Session, target: &ConfiguredTarget, exec_root: &Path, workspace: &Path) -> Result<Option<RunfilesLayout>, SkyError> {
    let Some((executable, entries)) = executable_runfiles(target) else {
        return Ok(None);
    };
    let mut linked = Vec::with_capacity(entries.len());
    for (path, artifact) in &entries {
        linked.push((path.clone(), location(session, artifact, exec_root, workspace).await?));
    }
    let repos: BTreeSet<RepoName> = entries.iter().map(|(_, artifact)| artifact.owner.repo()).chain([RepoName::MAIN]).collect();
    Ok(Some(RunfilesLayout {
        executable: exec_root.join(executable.exec_path.to_string()),
        entries: linked,
        repo_mapping: repo_mapping(session, &repos).await?,
    }))
}

/// A target's executable, and its runfiles.
fn executable_runfiles(target: &ConfiguredTarget) -> Option<(Artifact, Vec<(String, Artifact)>)> {
    let value = target.target_value()?;
    let executable = FileValue::artifact(value.default_info("executable")?)?;
    let runfiles = Runfiles::from_value(value.default_info("default_runfiles")?)?;
    Some((executable, runfiles.entries(runfiles_repo_name(RepoName::MAIN))))
}

/// Where an artifact is: in the output tree, or in its repository's source
/// tree.
async fn location(session: &Session, artifact: &Artifact, exec_root: &Path, workspace: &Path) -> Result<PathBuf, SkyError> {
    if !artifact.is_source() {
        return Ok(exec_root.join(artifact.exec_path.to_string()));
    }
    let label = artifact.owner;
    let relative = label.package.path.join_path(bazel_analysis::name_path(label)).to_string();
    let root = session.evaluate(&RepoRootKey(label.repo())).await?;
    let base = match root.0 {
        Root::Workspace => workspace.to_path_buf(),
        Root::Dir(dir) => PathBuf::from(dir.as_str()),
    };
    Ok(base.join(relative))
}

/// The repository mapping manifest: how each repository with runfiles sees
/// the others that have runfiles, as `repo,apparent,canonical` lines.
async fn repo_mapping(session: &Session, repos: &BTreeSet<RepoName>) -> Result<String, SkyError> {
    let mut lines = Vec::new();
    for repo in repos {
        let mapping = session.evaluate(&RepoMappingKey(*repo)).await?;
        for (apparent, target) in &mapping.0.entries {
            if apparent.is_empty() || !repos.contains(target) {
                continue;
            }
            lines.push(format!("{},{apparent},{}", repo.as_str(), runfiles_repo_name(*target)));
        }
    }
    lines.sort();
    lines.dedup();
    Ok(lines.into_iter().map(|line| line + "\n").collect())
}
