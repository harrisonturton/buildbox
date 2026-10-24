//! How each repository is made.

use crate::star_value::StarValue;
use bazel_label::{Label, RepoName};
use skycodec::{Decode, Encode};
use skyframe::encoded_value;

/// Where a repository's files come from.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum RepoSpec {
    /// An archive, as registries give modules, with patches and overlays.
    Archive(ArchiveSpec),
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
    pub patches: Vec<RemoteFile>,
    pub patch_strip: i64,
    /// Files added on top of the archive, by path.
    pub overlay: Vec<(String, RemoteFile)>,
    /// The registry's MODULE.bazel, which replaces the archive's.
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
