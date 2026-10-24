//! Interned strings.

use crate::arena::{Arena, Text};
use crate::table::{Hashing, Table};
use std::cmp::Ordering;
use std::fmt;
use std::hash::BuildHasher;
use std::sync::LazyLock;

/// An interned string: a name such as a repository, package or target, or a
/// path segment.
///
/// Symbols compare and hash as integers. They order by their text, so sorted
/// output reads naturally.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Symbol(u32);

struct Symbols {
    arena: Arena,
    table: Table,
}

struct TextHashing;

impl Hashing for TextHashing {
    fn hash(&self, value: u64) -> u64 {
        hash_text(text(value).as_str())
    }
}

static SYMBOLS: LazyLock<Symbols> = LazyLock::new(|| {
    let symbols = Symbols {
        arena: Arena::new(),
        table: Table::new(),
    };
    let empty = symbols.intern("");
    assert_eq!(empty, Symbol::EMPTY);
    symbols
});

impl Symbols {
    fn intern(&self, s: &str) -> Symbol {
        let hash = hash_text(s);
        let is_match = |value| text(value).as_str() == s;
        let make = || self.arena.alloc_str(s).as_ptr() as u64;
        Symbol(self.table.intern(&TextHashing, hash, is_match, make))
    }
}

fn hash_text(s: &str) -> u64 {
    rustc_hash::FxBuildHasher.hash_one(s)
}

fn text(value: u64) -> Text {
    // SAFETY: every value in the symbol table came from `Text::as_ptr`.
    unsafe { Text::from_ptr(value as *const u8) }
}

impl Symbol {
    /// The empty string.
    pub const EMPTY: Symbol = Symbol(0);

    /// The symbol for `s`, interning it if needed.
    pub fn intern(s: &str) -> Symbol {
        SYMBOLS.intern(s)
    }

    /// The symbol for `s`, if it has been interned. Useful for lookups that
    /// shouldn't grow the table, such as matching user input.
    pub fn get(s: &str) -> Option<Symbol> {
        let hash = hash_text(s);
        SYMBOLS.table.find(hash, |value| text(value).as_str() == s).map(Symbol)
    }

    pub fn as_str(self) -> &'static str {
        text(SYMBOLS.table.value(self.0)).as_str()
    }

    pub fn is_empty(self) -> bool {
        self == Symbol::EMPTY
    }

    /// The symbol's id: dense, starting from zero, and stable for the life of
    /// the process (but not across processes).
    pub fn id(self) -> u32 {
        self.0
    }

    /// The symbol with the given id, if one has been interned.
    pub fn from_id(id: u32) -> Option<Symbol> {
        (id < SYMBOLS.table.len()).then_some(Symbol(id))
    }
}

/// How many symbols have been interned.
pub(crate) fn count() -> u32 {
    SYMBOLS.table.len()
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Symbol {
        Symbol::intern(s)
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Symbol) -> Ordering {
        if self == other {
            return Ordering::Equal;
        }
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Symbol) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// How many symbols are interned, and the memory they take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SymbolStats {
    pub symbols: u32,
    pub index_bytes: usize,
}

/// Report how many symbols are interned.
pub fn symbol_stats() -> SymbolStats {
    SymbolStats {
        symbols: SYMBOLS.table.len(),
        index_bytes: SYMBOLS.table.allocated_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_returns_the_same_symbol_for_equal_text() {
        let a = Symbol::intern("rules_rust");
        let b = Symbol::intern(&String::from("rules_rust"));
        assert_eq!(a, b);
        assert_eq!(a.as_str(), "rules_rust");
        assert_ne!(a, Symbol::intern("rules_cc"));
    }

    #[test]
    fn empty_is_preinterned() {
        assert_eq!(Symbol::intern(""), Symbol::EMPTY);
        assert!(Symbol::EMPTY.is_empty());
    }

    #[test]
    fn get_does_not_intern() {
        assert_eq!(Symbol::get("never interned anywhere"), None);
        let symbol = Symbol::intern("interned by get test");
        assert_eq!(Symbol::get("interned by get test"), Some(symbol));
    }

    #[test]
    fn symbols_order_by_text() {
        let mut symbols = vec![Symbol::intern("b"), Symbol::intern("c"), Symbol::intern("a")];
        symbols.sort();
        let texts: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    #[test]
    fn concurrent_interning_agrees() {
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..2000).map(|i| Symbol::intern(&format!("t{i}"))).collect::<Vec<_>>()))
            .collect();
        let results: Vec<Vec<Symbol>> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for result in &results[1..] {
            assert_eq!(result, &results[0]);
        }
    }
}
