//! The actions Bazel registers to lay out an executable rule's runfiles
//! tree: its repository mapping manifest, its source manifest, the tree,
//! and the file standing for the tree's files. bb lays out runfiles trees
//! as it runs their executables, so these are made for `aquery` to show,
//! after the rule's own actions, as Bazel shows them.

use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTarget, Executable, InputSet, RunfilesTree};
use bazel_label::RepoName;
use intern::{PathId, Symbol};
use std::collections::BTreeSet;
use std::path::Path;
use skyframe::{Compute, SkyError};

/// What the runfiles manifests' contents need.
pub(crate) struct ManifestContents<'a> {
    /// The repositories of the packages the executable depends on, whose
    /// code may look files up.
    pub sources: BTreeSet<RepoName>,
    /// The execution root, which the source manifest names files in.
    pub exec_root: &'a Path,
}

/// The runfiles actions of `target`, if it's an executable rule's, with
/// the contents of the manifests they write if `contents` says what they
/// need.
pub(crate) async fn runfiles_actions(compute: &impl Compute, target: &ConfiguredTarget, contents: Option<ManifestContents<'_>>) -> Result<Vec<Action>, SkyError> {
    let Some(executable) = &target.executable else {
        return Ok(Vec::new());
    };
    let files = Files::of(target, executable);
    let label = target.label;
    let tree = &executable.runfiles;
    let (repo_mapping, manifest) = match contents {
        Some(contents) => {
            let repo_mapping = bazel_analysis::repo_mapping_manifest(compute, tree.entries.iter().map(|(_, artifact)| *artifact), Some(contents.sources)).await?;
            (repo_mapping, source_manifest(tree, files.repo_mapping, contents.exec_root))
        }
        None => (String::new(), String::new()),
    };
    let mut runfiles: Vec<Artifact> = tree.entries.iter().map(|(_, artifact)| *artifact).collect();
    runfiles.extend([files.repo_mapping, files.tree_manifest]);
    Ok(vec![
        action("RepoMappingManifest", format!("Writing repo mapping manifest for {label}"), InputSet::default(), files.repo_mapping, write(repo_mapping)),
        action("SourceSymlinkManifest", format!("Creating source manifest for {label}"), InputSet::default(), files.manifest, write(manifest)),
        action(
            "SymlinkTree",
            format!("Creating runfiles tree {}", tree.dir),
            InputSet::of(vec![files.manifest]),
            files.tree_manifest,
            ActionKind::SymlinkTree { env: target.config.get().options.action_env.clone() },
        ),
        action("RunfilesTree", format!("runfiles for {label}"), InputSet::of(runfiles), files.tree, ActionKind::RunfilesTree),
    ])
}

/// The files the runfiles actions write.
struct Files {
    /// `<executable>.repo_mapping`.
    repo_mapping: Artifact,
    /// `<executable>.runfiles_manifest`.
    manifest: Artifact,
    /// `<executable>.runfiles/MANIFEST`.
    tree_manifest: Artifact,
    /// `<executable>.runfiles`.
    tree: Artifact,
}

impl Files {
    fn of(target: &ConfiguredTarget, executable: &Executable) -> Files {
        let file = |path: PathId| Artifact {
            exec_path: path,
            owner: target.label,
            config: Some(target.config),
            is_directory: false,
        };
        let beside = |suffix: &str| file(bazel_analysis::parse_path(&format!("{}{suffix}", executable.file.exec_path)));
        let dir = executable.runfiles.dir;
        Files {
            repo_mapping: beside(".repo_mapping"),
            manifest: beside(".runfiles_manifest"),
            tree_manifest: file(dir.join_path(bazel_analysis::parse_path("MANIFEST"))),
            tree: file(dir),
        }
    }
}

/// The source manifest of a runfiles tree: a line for each file, its path
/// in the tree and the file's in the execution root, sorted, the
/// repository mapping included. As in Bazel, a tree with nothing of the
/// main repository has an empty file in its directory, so that the
/// directory exists.
fn source_manifest(tree: &RunfilesTree, repo_mapping: Artifact, exec_root: &Path) -> String {
    let absolute = |artifact: &Artifact| exec_root.join(artifact.exec_path.to_string()).display().to_string();
    let mut lines: Vec<String> = tree.entries.iter().map(|(path, artifact)| format!("{path} {}\n", absolute(artifact))).collect();
    lines.push(format!("_repo_mapping {}\n", absolute(&repo_mapping)));
    let main = format!("{}/", bazel_analysis::runfiles_repo_name(RepoName::MAIN));
    if !tree.entries.iter().any(|(path, _)| path.to_string().starts_with(&main)) {
        lines.push(format!("{main}.runfile \n"));
    }
    lines.sort();
    lines.concat()
}

fn write(content: String) -> ActionKind {
    ActionKind::Write {
        content,
        executable: false,
        from_args: false,
    }
}

fn action(mnemonic: &str, progress_message: String, inputs: InputSet, output: Artifact, kind: ActionKind) -> Action {
    Action {
        mnemonic: Symbol::intern(mnemonic),
        progress_message,
        inputs,
        runfiles: Vec::new(),
        outputs: vec![output],
        kind,
    }
}
