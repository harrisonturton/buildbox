//! Fetching what external repositories are made of: downloading files,
//! extracting archives and applying patches.
//!
//! Downloads are verified against the checksum they're expected to have, and
//! kept in a cache addressed by their SHA-256, so each file is downloaded
//! once per machine whatever workspace or repository asks for it.

mod credentials;
mod download;
mod extract;
mod integrity;
mod netrc;
mod patch;

pub use credentials::CredentialHelpers;
pub use download::{DownloadCache, DownloadError, RequestHeaders};
pub use extract::{ArchiveType, ExtractError, extract};
pub use integrity::{Integrity, IntegrityError, sha256_hex_of};
pub use netrc::{Credentials, Netrc};
pub use patch::{PatchError, apply_patch, apply_to_file};
