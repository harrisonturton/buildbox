//! Bazel's `glob()`: the files of a package matching patterns.

use crate::{DirListing, DirListingKey, EntryKind, RootedPath};
use intern::{PathId, Symbol};
use rustc_hash::FxHashSet;
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::sync::Arc;

/// A parsed glob pattern: path segments that are literal names, wildcards
/// using `*` and `?`, or `**`, which matches any number of directories.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct GlobPattern {
    segments: Vec<Segment>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
enum Segment {
    Literal(Symbol),
    Wildcard(String),
    Recursive,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid glob pattern '{pattern}': {reason}")]
pub struct GlobError {
    pub pattern: String,
    pub reason: &'static str,
}

impl GlobPattern {
    pub fn parse(pattern: &str) -> Result<GlobPattern, GlobError> {
        let invalid = |reason| GlobError {
            pattern: pattern.to_string(),
            reason,
        };
        if pattern.is_empty() {
            return Err(invalid("pattern is empty"));
        }
        if pattern.starts_with('/') {
            return Err(invalid("pattern can't be absolute"));
        }
        let mut segments = Vec::new();
        for segment in pattern.split('/') {
            let parsed = match segment {
                "" => return Err(invalid("pattern can't contain empty segments")),
                "." | ".." => return Err(invalid("pattern can't contain '.' or '..' segments")),
                "**" => Segment::Recursive,
                _ if segment.contains("**") => return Err(invalid("'**' must be a whole segment")),
                _ if segment.contains(['*', '?']) => Segment::Wildcard(segment.to_string()),
                _ => Segment::Literal(Symbol::intern(segment)),
            };
            segments.push(parsed);
        }
        Ok(GlobPattern { segments })
    }

    /// Whether a package-relative path matches the pattern.
    pub fn matches(&self, path: PathId) -> bool {
        let names = path.segments();
        matches_from(&self.segments, &names)
    }
}

fn matches_from(segments: &[Segment], names: &[Symbol]) -> bool {
    let Some((segment, rest)) = segments.split_first() else {
        return names.is_empty();
    };
    if let Segment::Recursive = segment {
        return (0..=names.len()).any(|skip| matches_from(rest, &names[skip..]));
    }
    let Some((name, names)) = names.split_first() else {
        return false;
    };
    segment.matches(*name) && matches_from(rest, names)
}

impl Segment {
    fn matches(&self, name: Symbol) -> bool {
        match self {
            Segment::Literal(literal) => *literal == name,
            Segment::Wildcard(pattern) => wildcard_matches(pattern.as_bytes(), name.as_str().as_bytes()),
            Segment::Recursive => true,
        }
    }
}

fn wildcard_matches(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|skip| wildcard_matches(rest, &name[skip..])),
        Some((b'?', rest)) => !name.is_empty() && wildcard_matches(rest, &name[1..]),
        Some((c, rest)) => name.first() == Some(c) && wildcard_matches(rest, &name[1..]),
    }
}

/// The files, and optionally directories, of the package in `package` that
/// match any of `include` and none of `exclude`. Doesn't look inside
/// subpackages: directories with a BUILD file of their own.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct Glob {
    pub package: RootedPath,
    pub include: Vec<GlobPattern>,
    pub exclude: Vec<GlobPattern>,
    pub exclude_directories: bool,
}

/// What a [`Glob`] matched.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GlobResult {
    /// The matching paths, relative to the package, sorted.
    pub matches: Vec<PathId>,
    /// The indexes of include patterns that matched nothing, for callers
    /// that disallow empty globs.
    pub empty_includes: Vec<u32>,
}

encoded_value!(GlobResult);

impl Key for Glob {
    type Value = GlobResult;
    const KIND: &'static str = "GLOB";

    async fn compute(&self, ctx: &Ctx) -> Result<GlobResult, SkyError> {
        let mut matches = Vec::new();
        let mut empty_includes = Vec::new();
        for (index, pattern) in self.include.iter().enumerate() {
            let found = walk(ctx, self.package, pattern, self.exclude_directories).await?;
            if found.is_empty() {
                empty_includes.push(index as u32);
            }
            matches.extend(found);
        }
        matches.retain(|path| !self.exclude.iter().any(|pattern| pattern.matches(*path)));
        matches.sort();
        matches.dedup();
        Ok(GlobResult { matches, empty_includes })
    }
}

/// A point in matching a pattern: a directory, relative to the package, and
/// the index of the next segment to match in it.
type State = (PathId, usize);

/// Match one pattern by listing directories level by level, so the listings
/// at each level are read in parallel.
async fn walk(ctx: &Ctx, package: RootedPath, pattern: &GlobPattern, exclude_directories: bool) -> Result<Vec<PathId>, SkyError> {
    let segments = &pattern.segments;
    let mut found = Vec::new();
    let mut seen: FxHashSet<State> = FxHashSet::default();
    let mut frontier: Vec<State> = vec![(PathId::ROOT, 0)];
    seen.insert((PathId::ROOT, 0));

    while !frontier.is_empty() {
        let listings = list_all(ctx, package, &frontier).await?;
        let mut next = Vec::new();
        for ((dir, index), listing) in frontier.iter().zip(listings) {
            if !dir.is_root() && is_package(&listing) {
                continue;
            }
            step(&segments[*index..], *dir, *index, &listing, exclude_directories, &mut found, &mut next);
        }
        next.retain(|state| seen.insert(*state));
        frontier = next;
    }
    Ok(found)
}

/// Match the segments starting at `index` against one directory's entries,
/// adding matches to `found` and directories to descend into to `next`.
fn step(
    segments: &[Segment],
    dir: PathId,
    index: usize,
    listing: &DirListing,
    exclude_directories: bool,
    found: &mut Vec<PathId>,
    next: &mut Vec<State>,
) {
    let (segment, rest) = segments.split_first().expect("states always have a segment left");
    let is_last = rest.is_empty();

    if let Segment::Recursive = segment {
        if is_last {
            add_everything(dir, listing, exclude_directories, found, next, index);
            return;
        }
        step(rest, dir, index + 1, listing, exclude_directories, found, next);
        for (name, kind) in &listing.entries {
            if *kind == EntryKind::Directory {
                next.push((dir.join(*name), index));
            }
        }
        return;
    }

    for (name, kind) in &listing.entries {
        if !segment.matches(*name) {
            continue;
        }
        let path = dir.join(*name);
        if is_last {
            if *kind == EntryKind::File || (*kind == EntryKind::Directory && !exclude_directories) {
                found.push(path);
            }
        } else if *kind == EntryKind::Directory {
            next.push((path, index + 1));
        }
    }
}

/// A trailing `**`: everything beneath `dir`.
fn add_everything(dir: PathId, listing: &DirListing, exclude_directories: bool, found: &mut Vec<PathId>, next: &mut Vec<State>, index: usize) {
    for (name, kind) in &listing.entries {
        let path = dir.join(*name);
        match kind {
            EntryKind::File => found.push(path),
            EntryKind::Directory => {
                next.push((path, index));
                if !exclude_directories {
                    found.push(path);
                }
            }
            EntryKind::Other => {}
        }
    }
}

/// Whether a directory has a BUILD file, making it a package of its own.
fn is_package(listing: &DirListing) -> bool {
    ["BUILD.bazel", "BUILD"]
        .iter()
        .any(|name| listing.get(Symbol::intern(name)) == Some(EntryKind::File))
}

async fn list_all(ctx: &Ctx, package: RootedPath, states: &[State]) -> Result<Vec<Arc<DirListing>>, SkyError> {
    let keys: Vec<DirListingKey> = states
        .iter()
        .map(|(dir, _)| DirListingKey(RootedPath::new(package.root, package.path.join_path(*dir))))
        .collect();
    ctx.compute_many(&keys).await.into_iter().collect()
}
