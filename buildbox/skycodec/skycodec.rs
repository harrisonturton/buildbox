//! A compact binary encoding for build graph keys and values, and the
//! fingerprints that identify them.
//!
//! The encoding has no field names or tags: a type's [`Encode`] and
//! [`Decode`] impls write and read its fields in a fixed order. Integers are
//! variable-length, so small numbers (most lengths, counts and ids) take a
//! byte. Interned [`Symbol`]s and [`PathId`]s are written as their ids, which
//! are only meaningful within one process; encodings that leave the process
//! must translate them.
//!
//! `#[derive(Encode, Decode)]` implements both for structs and enums whose
//! fields implement them.

mod decode;
mod encode;
mod fingerprint;

pub use decode::{Decode, DecodeError, Decoder};
pub use encode::{Encode, Encoder};
pub use fingerprint::{Fingerprint, Fingerprinter};

pub use intern::{PathId, Symbol};
pub use skycodec_derive::{Decode, Encode};

/// Encode a value into a new buffer.
pub fn to_bytes<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    let mut encoder = Encoder::new();
    value.encode(&mut encoder);
    encoder.into_bytes()
}

/// Decode a value that makes up the whole of `bytes`.
pub fn from_bytes<T: Decode>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut decoder = Decoder::new(bytes);
    let value = T::decode(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}
