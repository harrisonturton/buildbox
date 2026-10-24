//! Interned strings and paths: each distinct value is stored once, for the
//! life of the process, and referred to by a 4-byte id.
//!
//! The build graph refers to the same names over and over: repositories,
//! packages, targets and path segments. Interning makes each reference a
//! `u32` that compares and hashes as an integer, and keeps one copy of the
//! text however many workspaces use it. Interned values are never freed.

mod arena;
mod blob;
mod path;
mod segvec;
mod symbol;
mod table;

pub use blob::BlobTable;
pub use path::{path_stats, PathError, PathId, PathStats};
pub use symbol::{symbol_stats, Symbol, SymbolStats};
