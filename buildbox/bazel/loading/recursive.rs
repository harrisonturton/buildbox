//! Finding the packages beneath a directory, for patterns like `//foo/...`.

use crate::package::BUILD_FILE_NAMES;
use crate::RepoRootKey;
use bazel_fs::{DirListingKey, EntryKind, FileContentKey, RootedPath};
use bazel_label::{PackageId, RepoName};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Compute, Ctx, Key, SkyError};

/// What is beneath one directory: whether it's a package, and which of its
/// subdirectories have packages beneath them.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PackagesUnderDirectory {
    pub is_package: bool,
    pub children: Vec<Symbol>,
}

encoded_value!(PackagesUnderDirectory);

impl PackagesUnderDirectory {
    fn has_packages(&self) -> bool {
        self.is_package || !self.children.is_empty()
    }
}

/// Finds the packages beneath a directory of a repository. Each directory
/// is a key of its own, so a change deep in the tree only recomputes the
/// directories above it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PackagesUnderDirectoryKey {
    pub repo: RepoName,
    pub dir: PathId,
}

impl Key for PackagesUnderDirectoryKey {
    type Value = PackagesUnderDirectory;
    const KIND: &'static str = "PACKAGES_UNDER_DIRECTORY";

    async fn compute(&self, ctx: &Ctx) -> Result<PackagesUnderDirectory, SkyError> {
        let root = ctx.compute(&RepoRootKey(self.repo)).await?.0;
        let listing = ctx.compute(&DirListingKey(RootedPath::new(root, self.dir))).await?;
        let ignored = ctx.compute(&IgnoredDirectoriesKey(self.repo)).await?;
        let is_package = BUILD_FILE_NAMES
            .iter()
            .any(|name| listing.get(Symbol::intern(name)) == Some(EntryKind::File));

        let subdirs: Vec<Symbol> = listing
            .entries
            .iter()
            .filter(|(name, kind)| *kind == EntryKind::Directory && !is_ignored(self.repo, self.dir, *name, &ignored.0))
            .map(|(name, _)| *name)
            .collect();
        let keys: Vec<PackagesUnderDirectoryKey> = subdirs
            .iter()
            .map(|name| PackagesUnderDirectoryKey {
                repo: self.repo,
                dir: self.dir.join(*name),
            })
            .collect();
        let mut children = Vec::new();
        for (name, under) in subdirs.into_iter().zip(ctx.compute_many(&keys).await) {
            if under?.has_packages() {
                children.push(name);
            }
        }
        Ok(PackagesUnderDirectory { is_package, children })
    }
}

/// Directories that are never searched for packages: version control
/// metadata, Bazel's convenience symlinks, and what `.bazelignore` lists.
fn is_ignored(repo: RepoName, dir: PathId, name: Symbol, ignored: &[PathId]) -> bool {
    let name_str = name.as_str();
    if name_str == ".git" || name_str == ".hg" || name_str == ".svn" {
        return true;
    }
    if dir.is_root() && repo.is_main() && name_str.starts_with("bazel-") {
        return true;
    }
    let path = dir.join(name);
    ignored.contains(&path)
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct IgnoredDirectories(pub Vec<PathId>);

encoded_value!(IgnoredDirectories);

/// The directories a repository's `.bazelignore` lists.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct IgnoredDirectoriesKey(pub RepoName);

impl Key for IgnoredDirectoriesKey {
    type Value = IgnoredDirectories;
    const KIND: &'static str = "IGNORED_DIRECTORIES";

    async fn compute(&self, ctx: &Ctx) -> Result<IgnoredDirectories, SkyError> {
        let root = ctx.compute(&RepoRootKey(self.0)).await?.0;
        let path = PathId::parse(".bazelignore").expect("a valid path");
        let content = ctx.compute(&FileContentKey(RootedPath::new(root, path))).await?;
        let Some(bytes) = &content.bytes else {
            return Ok(IgnoredDirectories(Vec::new()));
        };
        let text = String::from_utf8_lossy(bytes);
        let mut ignored = Vec::new();
        for line in text.lines() {
            let line = line.trim().trim_end_matches('/');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Ok(path) = PathId::parse(line) {
                ignored.push(path);
            }
        }
        Ok(IgnoredDirectories(ignored))
    }
}

/// Every package beneath `prefix` in `repo`, including `prefix` itself,
/// sorted.
pub async fn packages_beneath(compute: &impl Compute, repo: RepoName, prefix: PathId) -> Result<Vec<PackageId>, SkyError> {
    let mut packages = Vec::new();
    let mut frontier = vec![prefix];
    while !frontier.is_empty() {
        let keys: Vec<PackagesUnderDirectoryKey> = frontier.iter().map(|dir| PackagesUnderDirectoryKey { repo, dir: *dir }).collect();
        let results = compute.compute_many(&keys).await;
        let mut next = Vec::new();
        for (dir, under) in frontier.iter().zip(results) {
            let under = under?;
            if under.is_package {
                packages.push(PackageId::new(repo, *dir));
            }
            next.extend(under.children.iter().map(|child| dir.join(*child)));
        }
        frontier = next;
    }
    packages.sort();
    Ok(packages)
}
