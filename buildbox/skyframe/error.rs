//! Errors computing values.

use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use std::fmt;
use std::sync::Arc;

/// Why a value couldn't be computed.
///
/// Errors are results like any other: an error computed for a key is kept
/// and returned again until the key's dependencies change. Errors that
/// depend on something outside the graph, or on timing, are
/// [transient](ErrorKind::Transient) and are computed again by the next
/// evaluation instead.
#[derive(Clone, PartialEq, Eq)]
pub struct SkyError(Arc<ErrorInner>);

#[derive(PartialEq, Eq)]
struct ErrorInner {
    kind: ErrorKind,
    message: String,
    /// What failed first, as the layer that made the error names it, kept
    /// as the error passes to the keys that depend on it.
    origin: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Computing the value failed, and would fail again with the same inputs.
    Failed,
    /// Computing the value failed for a reason that may not recur.
    Transient,
    /// The key depends on itself.
    Cycle,
}

impl SkyError {
    pub fn new(message: impl Into<String>) -> SkyError {
        SkyError::with_kind(ErrorKind::Failed, message)
    }

    pub fn transient(message: impl Into<String>) -> SkyError {
        SkyError::with_kind(ErrorKind::Transient, message)
    }

    /// A cycle through the keys described in `path`, which starts and ends
    /// with the same key.
    pub fn cycle(path: &[String]) -> SkyError {
        SkyError::with_kind(ErrorKind::Cycle, format!("cycle in dependency graph: {}", path.join(" -> ")))
    }

    pub fn with_kind(kind: ErrorKind, message: impl Into<String>) -> SkyError {
        SkyError(Arc::new(ErrorInner {
            kind,
            message: message.into(),
            origin: None,
        }))
    }

    /// The same error, saying what failed first.
    pub fn with_origin(&self, origin: impl Into<String>) -> SkyError {
        SkyError(Arc::new(ErrorInner {
            kind: self.kind(),
            message: self.message().to_string(),
            origin: Some(origin.into()),
        }))
    }

    /// What failed first, if the error says.
    pub fn origin(&self) -> Option<&str> {
        self.0.origin.as_deref()
    }

    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// The same error, with `context` saying what was being attempted.
    pub fn context(&self, context: impl fmt::Display) -> SkyError {
        SkyError(Arc::new(ErrorInner {
            kind: self.kind(),
            message: format!("{context}: {}", self.message()),
            origin: self.0.origin.clone(),
        }))
    }

    /// Whether the error may be kept as the key's result.
    pub fn is_cacheable(&self) -> bool {
        self.kind() == ErrorKind::Failed
    }
}

impl fmt::Display for SkyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl fmt::Debug for SkyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SkyError({:?}, {:?})", self.kind(), self.message())
    }
}

impl std::error::Error for SkyError {}

impl Encode for SkyError {
    fn encode(&self, e: &mut Encoder) {
        let kind = match self.kind() {
            ErrorKind::Failed => 0,
            ErrorKind::Transient => 1,
            ErrorKind::Cycle => 2,
        };
        e.u8(kind);
        e.str(self.message());
        e.str(self.origin().unwrap_or_default());
    }
}

impl Decode for SkyError {
    fn decode(d: &mut Decoder<'_>) -> Result<SkyError, DecodeError> {
        let position = d.position();
        let kind = match d.u8()? {
            0 => ErrorKind::Failed,
            1 => ErrorKind::Transient,
            2 => ErrorKind::Cycle,
            value => {
                return Err(DecodeError::Invalid {
                    what: "error kind",
                    value: value as u64,
                    position,
                })
            }
        };
        let error = SkyError::with_kind(kind, d.str()?);
        let origin = d.str()?;
        Ok(match origin.is_empty() {
            true => error,
            false => error.with_origin(origin),
        })
    }
}
