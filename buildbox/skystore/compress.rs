//! Compressing stored bytes with zstd, with or without a dictionary.

use crate::StoreError;
use std::sync::{Arc, Mutex, MutexGuard};
use zstd::zstd_safe::{self, CCtx, CDict, CParameter, DCtx, DDict, DParameter, FrameFormat};

/// The zstd level values are compressed at: fast, since values are written as
/// they are computed, while still compressing well with a dictionary.
const LEVEL: i32 = 3;

/// Values shorter than this are stored as they are; frame overhead would
/// outweigh what compression saves.
pub(crate) const MIN_COMPRESS_LEN: usize = 24;

/// The largest value the store holds. Lengths beyond this can only come from
/// corrupt data, and are rejected before allocating for them.
const MAX_VALUE_LEN: usize = 1 << 30;

/// How a stored value's bytes were encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Codec {
    Raw,
    Zstd,
    ZstdDict,
}

impl Codec {
    pub(crate) fn from_u8(value: u8) -> Option<Codec> {
        match value {
            0 => Some(Codec::Raw),
            1 => Some(Codec::Zstd),
            2 => Some(Codec::ZstdDict),
            _ => None,
        }
    }
}

/// A zstd dictionary trained on values of one kind, prepared for both
/// compressing and decompressing.
pub(crate) struct Dictionary {
    cdict: CDict<'static>,
    ddict: DDict<'static>,
    content: Box<[u8]>,
}

// zstd only reads prepared dictionaries, so they can be shared between threads.
unsafe impl Send for Dictionary {}
unsafe impl Sync for Dictionary {}

impl Dictionary {
    /// Train a dictionary of at most `max_len` bytes from samples, or return
    /// `None` if zstd can't find enough in common between them.
    pub(crate) fn train(samples: &[Vec<u8>], max_len: usize) -> Option<Dictionary> {
        let content = zstd::dict::from_samples(samples, max_len).ok()?;
        Some(Dictionary::from_content(content))
    }

    /// The dictionary with this content, as [`Dictionary::content`] gave it.
    pub(crate) fn from_content(content: Vec<u8>) -> Dictionary {
        Dictionary {
            cdict: CDict::create(&content, LEVEL),
            ddict: DDict::create(&content),
            content: content.into_boxed_slice(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.content.len()
    }

    pub(crate) fn content(&self) -> &[u8] {
        &self.content
    }
}

/// zstd's working state for compressing and decompressing. Each can grow
/// to megabytes, so a few are shared by every thread rather than each
/// thread having its own.
struct Contexts {
    plain: CCtx<'static>,
    with_dict: CCtx<'static>,
    decompress_plain: DCtx<'static>,
    decompress_with_dict: DCtx<'static>,
}

// zstd contexts can move between threads; they're only used by one at a
// time.
unsafe impl Send for Contexts {}

/// How many idle contexts to keep for reuse.
const POOLED_CONTEXTS: usize = 4;

static POOL: Mutex<Vec<Contexts>> = Mutex::new(Vec::new());

fn pool() -> MutexGuard<'static, Vec<Contexts>> {
    POOL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `f` with contexts from the pool, returning them afterwards.
fn with_contexts<R>(f: impl FnOnce(&mut Contexts) -> R) -> R {
    let mut contexts = pool().pop().unwrap_or_else(Contexts::new);
    let result = f(&mut contexts);
    let mut pool = pool();
    if pool.len() < POOLED_CONTEXTS {
        pool.push(contexts);
    }
    result
}

/// The memory the pooled contexts hold.
pub(crate) fn context_bytes() -> usize {
    pool()
        .iter()
        .map(|c| c.plain.sizeof() + c.with_dict.sizeof() + c.decompress_plain.sizeof() + c.decompress_with_dict.sizeof())
        .sum()
}

impl Contexts {
    /// Frames leave out everything the store records itself: the magic
    /// number, content size, dictionary id and checksum.
    fn new() -> Contexts {
        let mut plain = CCtx::create();
        let mut with_dict = CCtx::create();
        for cctx in [&mut plain, &mut with_dict] {
            let _ = cctx.set_parameter(CParameter::CompressionLevel(LEVEL));
            let _ = cctx.set_parameter(CParameter::Format(FrameFormat::Magicless));
            let _ = cctx.set_parameter(CParameter::ContentSizeFlag(false));
            let _ = cctx.set_parameter(CParameter::ChecksumFlag(false));
            let _ = cctx.set_parameter(CParameter::DictIdFlag(false));
        }
        let mut decompress_plain = DCtx::create();
        let mut decompress_with_dict = DCtx::create();
        for dctx in [&mut decompress_plain, &mut decompress_with_dict] {
            let _ = dctx.set_parameter(DParameter::Format(FrameFormat::Magicless));
        }
        Contexts {
            plain,
            with_dict,
            decompress_plain,
            decompress_with_dict,
        }
    }
}

/// Compress `bytes`, returning the codec used and the encoded bytes, which
/// start with the uncompressed length. Falls back to [`Codec::Raw`] when
/// compression doesn't make the value smaller.
pub(crate) fn compress(bytes: &[u8], dictionary: Option<&Arc<Dictionary>>) -> (Codec, Vec<u8>) {
    if bytes.len() < MIN_COMPRESS_LEN {
        return (Codec::Raw, bytes.to_vec());
    }

    let mut out = Vec::with_capacity(zstd_safe::compress_bound(bytes.len()) + 10);
    write_len(&mut out, bytes.len());
    let header = out.len();
    let mut frame = Vec::with_capacity(zstd_safe::compress_bound(bytes.len()));
    let pledged = Some(bytes.len() as u64);
    let result = with_contexts(|contexts| match dictionary {
        Some(dictionary) => {
            let cctx = &mut contexts.with_dict;
            cctx.ref_cdict(&dictionary.cdict)
                .and_then(|_| cctx.set_pledged_src_size(pledged))
                .and_then(|_| cctx.compress2(&mut frame, bytes))
        }
        None => {
            let cctx = &mut contexts.plain;
            cctx.set_pledged_src_size(pledged).and_then(|_| cctx.compress2(&mut frame, bytes))
        }
    });

    if result.is_err() || header + frame.len() >= bytes.len() {
        return (Codec::Raw, bytes.to_vec());
    }
    out.extend_from_slice(&frame);
    let codec = if dictionary.is_some() { Codec::ZstdDict } else { Codec::Zstd };
    (codec, out)
}

/// The uncompressed length of bytes [`compress`] returned.
pub(crate) fn raw_len(codec: Codec, stored: &[u8]) -> usize {
    match codec {
        Codec::Raw => stored.len(),
        Codec::Zstd | Codec::ZstdDict => read_len(stored).map_or(0, |(len, _)| len),
    }
}

/// Reverse [`compress`].
pub(crate) fn decompress(codec: Codec, stored: &[u8], dictionary: Option<&Dictionary>) -> Result<Vec<u8>, StoreError> {
    if codec == Codec::Raw {
        return Ok(stored.to_vec());
    }

    if codec == Codec::ZstdDict && dictionary.is_none() {
        return Err(StoreError::Corrupt("dictionary missing"));
    }
    let (len, frame) = read_len(stored).ok_or(StoreError::Corrupt("truncated length"))?;
    if len > MAX_VALUE_LEN {
        return Err(StoreError::Corrupt("length too large"));
    }
    let mut out = Vec::with_capacity(len);
    let result = with_contexts(|contexts| match (codec, dictionary) {
        (Codec::ZstdDict, Some(dictionary)) => {
            let dctx = &mut contexts.decompress_with_dict;
            dctx.ref_ddict(&dictionary.ddict).and_then(|_| dctx.decompress(&mut out, frame))
        }
        _ => contexts.decompress_plain.decompress(&mut out, frame),
    });
    result.map_err(|code| StoreError::Decompress(zstd_safe::get_error_name(code)))?;
    if out.len() != len {
        return Err(StoreError::Corrupt("decompressed length differs"));
    }
    Ok(out)
}

fn write_len(out: &mut Vec<u8>, mut len: usize) {
    while len >= 0x80 {
        out.push((len as u8) | 0x80);
        len >>= 7;
    }
    out.push(len as u8);
}

fn read_len(bytes: &[u8]) -> Option<(usize, &[u8])> {
    let mut len = 0usize;
    for (i, &byte) in bytes.iter().enumerate().take(5) {
        len |= ((byte & 0x7f) as usize) << (7 * i);
        if byte < 0x80 {
            return Some((len, &bytes[i + 1..]));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(i: usize) -> Vec<u8> {
        format!("{{\"label\":\"//pkg/sub{i}:target\",\"kind\":\"rust_library\",\"srcs\":[\"lib{i}.rs\"]}}").into_bytes()
    }

    #[test]
    fn short_values_are_raw() {
        let (codec, bytes) = compress(b"short", None);
        assert_eq!(codec, Codec::Raw);
        assert_eq!(decompress(codec, &bytes, None).unwrap(), b"short");
    }

    #[test]
    fn compressible_values_round_trip() {
        let value = b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".repeat(4);
        let (codec, bytes) = compress(&value, None);
        assert_eq!(codec, Codec::Zstd);
        assert!(bytes.len() < value.len() / 4);
        assert_eq!(decompress(codec, &bytes, None).unwrap(), value);
    }

    #[test]
    fn dictionaries_shrink_small_similar_values() {
        let samples: Vec<Vec<u8>> = (0..500).map(sample).collect();
        let dictionary = Arc::new(Dictionary::train(&samples, 4096).expect("samples have plenty in common"));

        let value = sample(1234);
        let (plain_codec, plain) = compress(&value, None);
        let (codec, with_dict) = compress(&value, Some(&dictionary));
        assert_eq!(codec, Codec::ZstdDict);
        assert!(with_dict.len() * 2 < plain.len().min(value.len()), "{} vs {}", with_dict.len(), plain.len());
        assert_eq!(decompress(codec, &with_dict, Some(&dictionary)).unwrap(), value);
        assert_eq!(decompress(plain_codec, &plain, None).unwrap(), value);
    }

    #[test]
    fn corrupt_data_is_an_error() {
        let value = b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".repeat(4);
        let (codec, mut bytes) = compress(&value, None);
        bytes.truncate(bytes.len() / 2);
        assert!(decompress(codec, &bytes, None).is_err());
    }
}
