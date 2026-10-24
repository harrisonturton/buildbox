//! Fetching what external repositories are made of: downloading files,
//! extracting archives and applying patches.
//!
//! Downloads are verified against the checksum they're expected to have, and
//! kept in a cache addressed by their SHA-256, so each file is downloaded
//! once per machine whatever workspace or repository asks for it.

mod download;
mod extract;
mod integrity;
mod patch;

pub use download::{DownloadCache, DownloadError};
pub use extract::{ArchiveType, ExtractError, extract};
pub use integrity::{Integrity, IntegrityError};
pub use patch::{PatchError, apply_patch};
