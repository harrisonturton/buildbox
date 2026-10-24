//! Artifacts: the files actions read and write.

use bazel_config::{BuildConfiguration, ConfigId};
use bazel_label::{Label, PackageId, RepoName};
use intern::PathId;
use skycodec::{Decode, Encode};
use std::fmt;

/// A file an action reads or writes: a source file, or one a rule's action
/// generates.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct Artifact {
    /// The path relative to the execution root, as actions see it.
    pub exec_path: PathId,
    /// The source file's target, or the rule whose action generates it.
    pub owner: Label,
    /// The configuration of the generating rule, or `None` for source
    /// files.
    pub config: Option<ConfigId>,
    pub is_directory: bool,
}

impl Artifact {
    /// A source file of a package.
    pub fn source(label: Label) -> Artifact {
        let path = repo_dir(label.package.repo).join_path(label.package.path).join_path(name_path(label));
        Artifact {
            exec_path: path,
            owner: label,
            config: None,
            is_directory: false,
        }
    }

    /// A file a rule generates, at `name` relative to the rule's package
    /// in its configuration's output directory.
    pub fn derived(owner: Label, config: &BuildConfiguration, name: PathId, is_directory: bool) -> Artifact {
        let package = package_dir(owner.package);
        let path = parse(&config.bin_dir()).join_path(package).join_path(name);
        Artifact {
            exec_path: path,
            owner,
            config: Some(config.id),
            is_directory,
        }
    }

    /// A generated file at `path` in the output directory, rather than in
    /// its owner's package, as `declare_shareable_artifact` declares them.
    pub fn derived_at_root(owner: Label, config: &BuildConfiguration, path: PathId, is_directory: bool) -> Artifact {
        Artifact {
            exec_path: parse(&config.bin_dir()).join_path(path),
            owner,
            config: Some(config.id),
            is_directory,
        }
    }

    pub fn is_source(&self) -> bool {
        self.config.is_none()
    }

    /// The directory the artifact's path is relative to, as `File.root`
    /// gives it: the execution root for source files, even another
    /// repository's, and the output directory for generated ones.
    pub fn root(&self) -> PathId {
        match self.config {
            Some(config) => parse(&config.get().bin_dir()),
            None => PathId::ROOT,
        }
    }

    /// The path relative to the root of the runfiles tree, as
    /// `File.short_path` gives it: `pkg/file`, or `../repo/pkg/file` for
    /// another repository's files.
    pub fn short_path(&self) -> String {
        let repo = self.owner.package.repo;
        if repo.is_main() {
            return self.repo_relative().to_string();
        }
        format!("../{repo}/{}", self.repo_relative())
    }

    /// The path in a runfiles tree, relative to its root: `_main/pkg/file`
    /// or `repo/pkg/file`.
    pub fn runfiles_path(&self) -> String {
        format!("{}/{}", runfiles_repo_name(self.owner.package.repo), self.repo_relative())
    }

    /// The path relative to the directory of the owner's repository: in the
    /// source tree, or in the output directory.
    fn repo_relative(&self) -> PathId {
        let repo = repo_dir(self.owner.package.repo);
        let base = match self.config {
            Some(config) => parse(&config.get().bin_dir()).join_path(repo),
            None => repo,
        };
        self.exec_path.strip_prefix(base).unwrap_or(self.exec_path)
    }
}

impl fmt::Debug for Artifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.exec_path)
    }
}

/// The name of a repository's directory in runfiles trees.
pub fn runfiles_repo_name(repo: RepoName) -> &'static str {
    if repo.is_main() { "_main" } else { repo.as_str() }
}

/// Where a repository's source files are, relative to the execution root.
pub fn repo_dir(repo: RepoName) -> PathId {
    if repo.is_main() {
        return PathId::ROOT;
    }
    parse("external").join(repo.symbol())
}

/// Where a package's files are relative to the execution root, or to an
/// output directory.
pub fn package_dir(package: PackageId) -> PathId {
    repo_dir(package.repo).join_path(package.path)
}

/// A target name, which may have slashes, as a path.
pub fn name_path(label: Label) -> PathId {
    parse(label.name.as_str())
}

/// Parse a path that is known to be valid.
pub fn parse(path: &str) -> PathId {
    PathId::parse(path).unwrap_or(PathId::ROOT)
}
