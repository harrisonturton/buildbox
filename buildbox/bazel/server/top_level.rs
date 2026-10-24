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
    /// Identifies the executable's runfiles tree, if the target has one.
    pub runfiles: Option<Fingerprint>,
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
            Some(entries) => Some(runfiles_fingerprint(&entries, &bazel_execution::repo_mapping(ctx, &entries).await?)),
            None => None,
        };
        let mut seen = BTreeSet::new();
        artifacts.retain(|artifact| !artifact.is_source() && seen.insert(*artifact));
        Ok(TopLevel { artifacts, runfiles })
    }
}

struct Flattened {
    artifacts: Vec<Artifact>,
    runfiles: Option<Vec<(String, Artifact)>>,
}

/// A target's default outputs and its executable's runfiles, flattened.
fn flatten(target: &ConfiguredTarget) -> Flattened {
    let Some(value) = target.target_value() else {
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
        runfiles: Some(entries),
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
