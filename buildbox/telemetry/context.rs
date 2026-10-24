use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

/// A trace's ID: 16 bytes, never all zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TraceId(pub [u8; 16]);

/// A span's ID: 8 bytes, never all zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanId(pub [u8; 8]);

/// Where a span sits: the trace it's part of, its own ID, and whether the
/// trace is sampled, so sent anywhere. Work in a trace that isn't is
/// recorded only to say so to the calls it makes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpanContext {
    pub trace: TraceId,
    pub span: SpanId,
    pub sampled: bool,
}

impl TraceId {
    /// A new, random ID.
    pub fn random() -> Self {
        let mut id = [0; 16];
        id[..8].copy_from_slice(&nonzero().to_be_bytes());
        id[8..].copy_from_slice(&random().to_be_bytes());
        Self(id)
    }
}

impl SpanId {
    /// A new, random ID.
    pub fn random() -> Self {
        Self(nonzero().to_be_bytes())
    }
}

impl SpanContext {
    /// The context a W3C `traceparent` header names,
    /// `<version>-<trace ID>-<parent ID>-<flags>` in lowercase hex, or None
    /// if it isn't one.
    pub fn from_traceparent(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('-');
        let (version, trace, span, flags) =
            (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
        let version = hex::<1>(version)?;
        let [flags] = hex::<1>(flags)?;
        // A later version may add parts; version 00 has exactly four, and
        // ff is never valid.
        if version == [0xff] || (version == [0] && parts.next().is_some()) {
            return None;
        }
        let trace = hex::<16>(trace)?;
        let span = hex::<8>(span)?;
        if trace == [0; 16] || span == [0; 8] {
            return None;
        }
        Some(Self {
            trace: TraceId(trace),
            span: SpanId(span),
            sampled: flags & 1 == 1,
        })
    }

    /// The `traceparent` header that continues this context in what it's
    /// sent to.
    pub fn traceparent(&self) -> String {
        let flags = if self.sampled { "01" } else { "00" };
        format!("00-{}-{}-{flags}", self.trace, self.span)
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// `N` bytes from exactly `2 * N` lowercase hex digits.
fn hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let digits = text.as_bytes();
    if digits.len() != 2 * N {
        return None;
    }
    let digit = |d: u8| match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'a'..=b'f' => Some(d - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0; N];
    for (byte, [high, low]) in bytes.iter_mut().zip(digits.as_chunks::<2>().0) {
        *byte = digit(*high)? << 4 | digit(*low)?;
    }
    Some(bytes)
}

/// 64 random bits. Each `RandomState` is keyed from the OS's randomness,
/// so hashing a counter with a new one is random enough for IDs, which need
/// only be unique.
fn random() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.finish()
}

fn nonzero() -> u64 {
    loop {
        let value = random();
        if value != 0 {
            return value;
        }
    }
}
