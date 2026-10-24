//! Saving the intern tables, so that ids written down in one process mean
//! the same in the next.
//!
//! Ids are dense and given in order, so a table is saved as its entries in
//! id order, and restored by interning them in that order, before anything
//! else is interned.

use crate::path::{self, PathId};
use crate::symbol::Symbol;

/// A restored entry got a different id than it had: something was interned
/// before the table was restored.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("interned id {got} where {expected} was saved")]
pub struct Mismatch {
    pub expected: u32,
    pub got: u32,
}

/// How many symbols have been interned.
pub fn symbol_count() -> u32 {
    crate::symbol::count()
}

/// The text of the symbols with ids from `start`, in id order.
pub fn symbols_from(start: u32) -> Vec<&'static str> {
    (start..symbol_count()).filter_map(Symbol::from_id).map(Symbol::as_str).collect()
}

/// Intern `texts` as the symbols with ids from `start`.
pub fn restore_symbols<'a>(start: u32, texts: impl IntoIterator<Item = &'a str>) -> Result<(), Mismatch> {
    for (expected, text) in (start..).zip(texts) {
        let got = Symbol::intern(text).id();
        if got != expected {
            return Err(Mismatch { expected, got });
        }
    }
    Ok(())
}

/// How many paths have been interned.
pub fn path_count() -> u32 {
    path::count()
}

/// The parent and name symbol of the paths with ids from `start`, in id
/// order. The root has no parent, given as `u32::MAX`.
pub fn paths_from(start: u32) -> Vec<(u32, u32)> {
    (start..path_count()).filter_map(PathId::from_id).map(path::raw_entry).collect()
}

/// Intern `entries`, as [`paths_from`] gives them, as the paths with ids
/// from `start`.
pub fn restore_paths(start: u32, entries: impl IntoIterator<Item = (u32, u32)>) -> Result<(), Mismatch> {
    for (expected, (parent, name)) in (start..).zip(entries) {
        let got = path::restore_entry(parent, name).id();
        if got != expected {
            return Err(Mismatch { expected, got });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_what_is_interned_keeps_ids() {
        let path = PathId::parse("snapshot/test/file").unwrap();
        let symbols = symbols_from(0);
        let paths = paths_from(0);
        assert_eq!(restore_symbols(0, symbols.iter().copied()), Ok(()));
        assert_eq!(restore_paths(0, paths), Ok(()));
        assert_eq!(PathId::parse("snapshot/test/file").unwrap(), path);
    }

    #[test]
    fn a_different_order_is_a_mismatch() {
        Symbol::intern("snapshot-a");
        let count = symbol_count();
        assert!(restore_symbols(count, ["snapshot-a"]).is_err());
    }
}
