//! Interned relative paths.

use crate::symbol::Symbol;
use crate::table::{Hashing, Table};
use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

/// An interned, normalized relative path, such as a package path or a file's
/// path within a repository.
///
/// Paths are stored as a tree: each is its parent and a final segment, both
/// ids, so every path costs eight bytes however long it is, and paths that
/// share a prefix share its storage. The empty path, [`PathId::ROOT`], is the
/// root of the tree.
///
/// Paths order as their text does.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathId(u32);

/// Why text isn't a normalized relative path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path {0:?} is absolute")]
    Absolute(String),

    #[error("path {0:?} has an empty, '.' or '..' segment")]
    NotNormalized(String),
}

const NO_PARENT: u32 = u32::MAX;

static PATHS: LazyLock<Table> = LazyLock::new(|| {
    let table = Table::new();
    let root = intern_entry(&table, NO_PARENT, Symbol::EMPTY);
    assert_eq!(root, PathId::ROOT);
    table
});

struct EntryHashing;

impl Hashing for EntryHashing {
    fn hash(&self, value: u64) -> u64 {
        hash_entry(value)
    }
}

fn pack(parent: u32, name: Symbol) -> u64 {
    ((parent as u64) << 32) | name.id() as u64
}

fn hash_entry(value: u64) -> u64 {
    value.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(26)
}

/// How many paths have been interned.
pub(crate) fn count() -> u32 {
    PATHS.len()
}

/// A path's parent's id, `NO_PARENT` for the root, and its name's.
pub(crate) fn raw_entry(path: PathId) -> (u32, u32) {
    let value = PATHS.value(path.0);
    ((value >> 32) as u32, value as u32)
}

/// Intern the path with this parent and name, as [`raw_entry`] gave them.
pub(crate) fn restore_entry(parent: u32, name: u32) -> PathId {
    let name = Symbol::from_id(name).unwrap_or(Symbol::EMPTY);
    intern_entry(&PATHS, parent, name)
}

fn intern_entry(table: &Table, parent: u32, name: Symbol) -> PathId {
    let value = pack(parent, name);
    PathId(table.intern(&EntryHashing, hash_entry(value), |v| v == value, || value))
}

impl PathId {
    /// The empty path.
    pub const ROOT: PathId = PathId(0);

    /// Parse a normalized relative path: segments separated by single `/`s,
    /// none of them `.` or `..`. The empty string is [`PathId::ROOT`].
    pub fn parse(text: &str) -> Result<PathId, PathError> {
        if text.is_empty() {
            return Ok(PathId::ROOT);
        }
        if text.starts_with('/') {
            return Err(PathError::Absolute(text.to_string()));
        }

        let mut path = PathId::ROOT;
        for segment in text.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(PathError::NotNormalized(text.to_string()));
            }
            path = path.join(Symbol::intern(segment));
        }
        Ok(path)
    }

    /// The path for `text` if it has been interned, without interning it.
    pub fn get(text: &str) -> Option<PathId> {
        if text.is_empty() {
            return Some(PathId::ROOT);
        }
        let mut path = PathId::ROOT;
        for segment in text.split('/') {
            let name = Symbol::get(segment)?;
            let value = pack(path.0, name);
            path = PathId(PATHS.find(hash_entry(value), |v| v == value)?);
        }
        Some(path)
    }

    /// This path with `name` appended. `name` must be a single segment.
    pub fn join(self, name: Symbol) -> PathId {
        intern_entry(&PATHS, self.0, name)
    }

    /// This path with the normalized relative path `rest` appended.
    pub fn join_path(self, rest: PathId) -> PathId {
        let mut path = self;
        for segment in rest.segments() {
            path = path.join(segment);
        }
        path
    }

    /// The path without its last segment, or `None` for the root.
    pub fn parent(self) -> Option<PathId> {
        let (parent, _) = self.entry();
        (parent != NO_PARENT).then_some(PathId(parent))
    }

    /// The last segment, or the empty symbol for the root.
    pub fn name(self) -> Symbol {
        self.entry().1
    }

    pub fn is_root(self) -> bool {
        self == PathId::ROOT
    }

    /// The segments, first to last.
    pub fn segments(self) -> Vec<Symbol> {
        let mut segments = Vec::new();
        let mut path = self;
        while let Some(parent) = path.parent() {
            segments.push(path.name());
            path = parent;
        }
        segments.reverse();
        segments
    }

    /// Whether `prefix` is this path or one of its ancestors.
    pub fn starts_with(self, prefix: PathId) -> bool {
        let mut path = self;
        loop {
            if path == prefix {
                return true;
            }
            match path.parent() {
                Some(parent) => path = parent,
                None => return false,
            }
        }
    }

    /// The rest of this path after `prefix`, if `prefix` is an ancestor.
    pub fn strip_prefix(self, prefix: PathId) -> Option<PathId> {
        let segments = self.segments();
        let skip = prefix.segments();
        if !segments.starts_with(&skip) {
            return None;
        }
        let mut rest = PathId::ROOT;
        for segment in &segments[skip.len()..] {
            rest = rest.join(*segment);
        }
        Some(rest)
    }

    /// The path's id: dense and stable for the life of the process.
    pub fn id(self) -> u32 {
        self.0
    }

    /// The path with the given id, if one has been interned.
    pub fn from_id(id: u32) -> Option<PathId> {
        (id < PATHS.len()).then_some(PathId(id))
    }

    fn entry(self) -> (u32, Symbol) {
        let value = PATHS.value(self.0);
        let parent = (value >> 32) as u32;
        let name = Symbol::from_id(value as u32).expect("path names are interned symbols");
        (parent, name)
    }
}

impl Ord for PathId {
    /// As their text orders, without writing it out.
    fn cmp(&self, other: &PathId) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }
        if self.parent() == other.parent() {
            return self.name().as_str().cmp(other.name().as_str());
        }
        let ours = self.segments();
        let theirs = other.segments();
        let common = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
        text_bytes(&ours[common..]).cmp(text_bytes(&theirs[common..]))
    }
}

/// The bytes of `segments` joined with `/`.
fn text_bytes(segments: &[Symbol]) -> impl Iterator<Item = u8> + '_ {
    segments.iter().enumerate().flat_map(|(i, segment)| {
        let separator: &[u8] = if i > 0 { b"/" } else { b"" };
        separator.iter().copied().chain(segment.as_str().bytes())
    })
}

impl PartialOrd for PathId {
    fn partial_cmp(&self, other: &PathId) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for PathId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, segment) in self.segments().iter().enumerate() {
            if i > 0 {
                f.write_str("/")?;
            }
            f.write_str(segment.as_str())?;
        }
        Ok(())
    }
}

impl fmt::Debug for PathId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string())
    }
}

/// How many paths are interned, and the memory their index takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PathStats {
    pub paths: u32,
    pub index_bytes: usize,
}

/// Report how many paths are interned.
pub fn path_stats() -> PathStats {
    PathStats {
        paths: PATHS.len(),
        index_bytes: PATHS.allocated_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(text: &str) -> PathId {
        PathId::parse(text).unwrap()
    }

    #[test]
    fn parse_round_trips() {
        for text in ["", "a", "foo/bar", "foo/bar/baz.rs"] {
            assert_eq!(path(text).to_string(), text);
        }
        assert_eq!(path("foo/bar"), path("foo").join(Symbol::intern("bar")));
    }

    #[test]
    fn parse_rejects_unnormalized_paths() {
        assert!(matches!(PathId::parse("/abs"), Err(PathError::Absolute(_))));
        for text in ["a//b", "a/./b", "../a", "a/"] {
            assert!(matches!(PathId::parse(text), Err(PathError::NotNormalized(_))), "{text}");
        }
    }

    #[test]
    fn paths_order_as_their_text() {
        let texts = ["", "a", "a-b", "a.b", "a/b", "a/b/c", "a/x", "ab", "b/a", "a.b/c", "a/b-c", "a/b/d"];
        for a in texts {
            for b in texts {
                assert_eq!(path(a).cmp(&path(b)), a.cmp(b), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn parent_and_name() {
        let p = path("foo/bar");
        assert_eq!(p.name().as_str(), "bar");
        assert_eq!(p.parent(), Some(path("foo")));
        assert_eq!(PathId::ROOT.parent(), None);
    }

    #[test]
    fn prefixes() {
        let p = path("a/b/c");
        assert!(p.starts_with(path("a/b")));
        assert!(p.starts_with(PathId::ROOT));
        assert!(!p.starts_with(path("a/c")));
        assert_eq!(p.strip_prefix(path("a")), Some(path("b/c")));
        assert_eq!(p.strip_prefix(path("b")), None);
    }

    #[test]
    fn get_does_not_intern() {
        assert_eq!(PathId::get("never/interned/path"), None);
        let p = path("interned/by/get");
        assert_eq!(PathId::get("interned/by/get"), Some(p));
    }

    #[test]
    fn paths_order_as_text() {
        let mut paths = vec![path("a/b"), path("a-c"), path("a")];
        paths.sort();
        let texts: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        assert_eq!(texts, ["a", "a-c", "a/b"]);
    }
}
