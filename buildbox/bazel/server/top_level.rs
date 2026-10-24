//! What building a target at the top level builds, worked out once per
//! configured target, so that a build of unchanged targets doesn't flatten
//! their runfiles again.

use bazel_analysis::{Artifact, ConfiguredTarget, ConfiguredTargetKey, FileValue, Runfiles, runfiles_repo_name};
use bazel_label::RepoName;
use skycodec::{Decode, Encode, Fingerprint, Fingerprinter};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::BTreeSet;

/// The files a top-level target builds.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct TopLevelKey(pub ConfiguredTargetKey);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopLevel {
    /// The generated files to build: the target's default outputs, and if
    /// it's executable, its runfiles. Source files need no building.
    pub artifacts: Vec<Artifact>,
    /// The executable's runfiles tree, if the target has one.
    pub runfiles: Option<RunfilesTree>,
}

/// An executable's runfiles tree, recorded so a build can tell whether the
/// tree on disk is current without reading the target's providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RunfilesTree {
    /// The executable the tree belongs to.
    pub executable: Artifact,
    /// Identifies the tree's entries and repository mapping.
    pub fingerprint: Fingerprint,
}

encoded_value!(TopLevel);

impl Key for TopLevelKey {
    type Value = TopLevel;
    const KIND: &'static str = "TOP_LEVEL";

    async fn compute(&self, ctx: &Ctx) -> Result<TopLevel, SkyError> {
        let target = ctx.compute(&self.0).await?;
        let flattened = bazel_loading::blocking(move || flatten(&target)).await?;
        let Flattened { mut artifacts, runfiles } = flattened;
        let runfiles = match runfiles {
            Some((executable, entries)) => {
                let fingerprint = runfiles_fingerprint(&entries, &bazel_execution::repo_mapping(ctx, &entries).await?);
                Some(RunfilesTree { executable, fingerprint })
            }
            None => None,
        };
        let mut seen = BTreeSet::new();
        artifacts.retain(|artifact| !artifact.is_source() && seen.insert(*artifact));
        Ok(TopLevel { artifacts, runfiles })
    }
}

struct Flattened {
    artifacts: Vec<Artifact>,
    runfiles: Option<(Artifact, Vec<(String, Artifact)>)>,
}

/// A target's default outputs and its executable's runfiles, flattened.
fn flatten(target: &ConfiguredTarget) -> Flattened {
    let providers = target.providers().ok();
    let Some(value) = providers.as_ref().and_then(|providers| providers.get()) else {
        return Flattened {
            artifacts: Vec::new(),
            runfiles: None,
        };
    };
    let mut artifacts = crate::build::depset_files(value.default_info("files"));
    let executable = value.default_info("executable").and_then(FileValue::artifact);
    let runfiles = value.default_info("default_runfiles").and_then(Runfiles::from_value);
    let Some((executable, runfiles)) = executable.zip(runfiles) else {
        return Flattened { artifacts, runfiles: None };
    };
    let entries = runfiles.entries(runfiles_repo_name(RepoName::MAIN));
    artifacts.push(executable);
    artifacts.extend(entries.iter().map(|(_, artifact)| *artifact));
    Flattened {
        artifacts,
        runfiles: Some((executable, entries)),
    }
}

/// Identifies a runfiles tree by its entries and repository mapping.
fn runfiles_fingerprint(entries: &[(String, Artifact)], repo_mapping: &str) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(repo_mapping.as_bytes());
    for (path, artifact) in entries {
        fingerprinter.bytes(path.as_bytes()).bytes(artifact.exec_path.to_string().as_bytes());
    }
    fingerprinter.finish()
}
