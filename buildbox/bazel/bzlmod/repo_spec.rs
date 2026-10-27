//! How each repository is made.

use crate::module_file::GitSource;
use crate::star_value::StarValue;
use bazel_label::{Label, RepoName};
use skycodec::{Decode, Encode};
use skyframe::encoded_value;

/// Where a repository's files come from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum RepoSpec {
    /// An archive, as registries give modules, with patches and overlays.
    Archive(ArchiveSpec),
    /// A Git repository, as `git_repository` fetches it.
    Git(GitSpec),
    /// A directory on this machine, used as it is.
    Local { path: String },
    /// A directory of Bazel's own, such as `@bazel_tools`.
    Builtin { dir: String },
    /// A repository rule, run with the given attributes.
    RepoRule(RepoRuleSpec),
    /// `@local_config_platform`, describing this machine's platform.
    HostPlatform,
}

encoded_value!(RepoSpec);

/// A file to download, and its expected checksum.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RemoteFile {
    pub url: String,
    pub integrity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ArchiveSpec {
    pub urls: Vec<String>,
    pub integrity: String,
    pub strip_prefix: String,
    pub archive_type: String,
    /// The registry's patches, applied with `patch_strip`.
    pub patches: Vec<RemoteFile>,
    pub patch_strip: i64,
    /// Patches the root module's override gives, as labels of the main
    /// repository, applied with `local_patch_strip`.
    pub local_patches: Vec<String>,
    pub local_patch_strip: i64,
    /// Commands the override runs after patching, each with `bash -c`.
    pub patch_cmds: Vec<String>,
    /// Files added on top of the archive, by path.
    pub overlay: Vec<(String, RemoteFile)>,
    /// The registry's MODULE.bazel, which replaces the archive's.
    pub module_file: Option<String>,
}

/// A Git repository: its source, with the patches the root module's
/// override gives, and the registry's patches and MODULE.bazel if it's a
/// registry's module.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct GitSpec {
    pub source: GitSource,
    /// The registry's patches, applied with `registry_patch_strip` before
    /// the source's.
    pub registry_patches: Vec<RemoteFile>,
    pub registry_patch_strip: i64,
    /// The registry's MODULE.bazel, which replaces the repository's.
    pub module_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoRuleSpec {
    /// The `.bzl` file that exports the rule.
    pub bzl: Label,
    pub rule: String,
    pub attrs: Vec<(String, StarValue)>,
    /// The repository whose mapping resolves labels in `attrs`.
    pub base_repo: RepoName,
    /// The name the rule was called with, which `repository_ctx.original_name`
    /// gives.
    pub original_name: String,
}
