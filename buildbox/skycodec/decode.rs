//! Reading the encoding.

use crate::Fingerprint;
use intern::{PathId, Symbol};
use std::sync::Arc;

/// Reads encoded values from a buffer.
pub struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

/// A type that can be read with a [`Decoder`].
pub trait Decode: Sized {
    fn decode(d: &mut Decoder<'_>) -> Result<Self, DecodeError>;
}

/// Why encoded data couldn't be read. Data this process encoded never fails
/// to decode, so these mean corruption or a mismatched type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("unexpected end of data at byte {0}")]
    UnexpectedEnd(usize),

    #[error("integer at byte {0} is too large")]
    Overflow(usize),

    #[error("invalid UTF-8 at byte {0}")]
    InvalidUtf8(usize),

    #[error("unknown {kind} id {id}")]
    UnknownId { kind: &'static str, id: u32 },

    #[error("invalid {what} {value} at byte {position}")]
    Invalid {
        what: &'static str,
        value: u64,
        position: usize,
    },

    #[error("{0} bytes left over after decoding")]
    TrailingBytes(usize),
}

impl<'a> Decoder<'a> {
    pub fn new(bytes: &'a [u8]) -> Decoder<'a> {
        Decoder { bytes, position: 0 }
    }

    /// Check that every byte has been read.
    pub fn finish(self) -> Result<(), DecodeError> {
        let left = self.bytes.len() - self.position;
        if left > 0 {
            return Err(DecodeError::TrailingBytes(left));
        }
        Ok(())
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let start = self.position;
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            if shift == 63 && byte > 1 {
                return Err(DecodeError::Overflow(start));
            }
            value |= ((byte & 0x7f) as u64) << shift;
            if byte < 0x80 {
                return Ok(value);
            }
            shift += 7;
        }
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let start = self.position;
        let value = self.u64()?;
        u32::try_from(value).map_err(|_| DecodeError::Overflow(start))
    }

    pub fn usize(&mut self) -> Result<usize, DecodeError> {
        let start = self.position;
        let value = self.u64()?;
        usize::try_from(value).map_err(|_| DecodeError::Overflow(start))
    }

    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        let value = self.u64()?;
        Ok(((value >> 1) as i64) ^ -((value & 1) as i64))
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        let Some(&byte) = self.bytes.get(self.position) else {
            return Err(DecodeError::UnexpectedEnd(self.position));
        };
        self.position += 1;
        Ok(byte)
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(DecodeError::Invalid {
                what: "bool",
                value: value as u64,
                position,
            }),
        }
    }

    /// Bytes preceded by their length.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.usize()?;
        self.raw(len)
    }

    /// The next `len` bytes.
    pub fn raw(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.position.checked_add(len).filter(|&end| end <= self.bytes.len());
        let Some(end) = end else {
            return Err(DecodeError::UnexpectedEnd(self.bytes.len()));
        };
        let bytes = &self.bytes[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        let start = self.position;
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::InvalidUtf8(start))
    }

    pub fn symbol(&mut self) -> Result<Symbol, DecodeError> {
        let id = self.u32()?;
        Symbol::from_id(id).ok_or(DecodeError::UnknownId { kind: "symbol", id })
    }

    pub fn path(&mut self) -> Result<PathId, DecodeError> {
        let id = self.u32()?;
        PathId::from_id(id).ok_or(DecodeError::UnknownId { kind: "path", id })
    }

    /// Increasing `u32`s, as written by [`Encoder::sorted_u32s`](crate::Encoder::sorted_u32s).
    pub fn sorted_u32s(&mut self) -> Result<Vec<u32>, DecodeError> {
        let len = self.len()?;
        let mut values = Vec::with_capacity(len);
        let mut previous = 0u32;
        for _ in 0..len {
            let start = self.position;
            let gap = self.u32()?;
            previous = previous.checked_add(gap).ok_or(DecodeError::Overflow(start))?;
            values.push(previous);
        }
        Ok(values)
    }

    /// A sequence of values preceded by its length.
    pub fn seq<T: Decode>(&mut self) -> Result<Vec<T>, DecodeError> {
        let len = self.len()?;
        let mut values = Vec::with_capacity(len);
        for _ in 0..len {
            values.push(T::decode(self)?);
        }
        Ok(values)
    }

    /// A sequence length. Every element takes at least a byte, so a length
    /// longer than the remaining data is corrupt; rejecting it keeps corrupt
    /// data from causing a huge allocation.
    fn len(&mut self) -> Result<usize, DecodeError> {
        let position = self.position;
        let len = self.usize()?;
        if len > self.bytes.len() - self.position {
            return Err(DecodeError::Invalid {
                what: "length",
                value: len as u64,
                position,
            });
        }
        Ok(len)
    }
}

macro_rules! decode_uint {
    ($($ty:ty),*) => {$(
        impl Decode for $ty {
            fn decode(d: &mut Decoder<'_>) -> Result<$ty, DecodeError> {
                let start = d.position();
                let value = d.u64()?;
                <$ty>::try_from(value).map_err(|_| DecodeError::Overflow(start))
            }
        }
    )*};
}

decode_uint!(u16, u32, u64, usize);

impl Decode for u8 {
    fn decode(d: &mut Decoder<'_>) -> Result<u8, DecodeError> {
        d.u8()
    }
}

impl Decode for i64 {
    fn decode(d: &mut Decoder<'_>) -> Result<i64, DecodeError> {
        d.i64()
    }
}

impl Decode for i32 {
    fn decode(d: &mut Decoder<'_>) -> Result<i32, DecodeError> {
        let start = d.position();
        let value = d.i64()?;
        i32::try_from(value).map_err(|_| DecodeError::Overflow(start))
    }
}

impl Decode for bool {
    fn decode(d: &mut Decoder<'_>) -> Result<bool, DecodeError> {
        d.bool()
    }
}

impl Decode for String {
    fn decode(d: &mut Decoder<'_>) -> Result<String, DecodeError> {
        d.str().map(str::to_string)
    }
}

impl Decode for Symbol {
    fn decode(d: &mut Decoder<'_>) -> Result<Symbol, DecodeError> {
        d.symbol()
    }
}

impl Decode for PathId {
    fn decode(d: &mut Decoder<'_>) -> Result<PathId, DecodeError> {
        d.path()
    }
}

impl Decode for Fingerprint {
    fn decode(d: &mut Decoder<'_>) -> Result<Fingerprint, DecodeError> {
        let bytes = d.raw(Fingerprint::LEN)?;
        let bytes: [u8; Fingerprint::LEN] = bytes.try_into().expect("raw returns the length asked for");
        Ok(Fingerprint::from_bytes(bytes))
    }
}

impl<T: Decode> Decode for Vec<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Vec<T>, DecodeError> {
        d.seq()
    }
}

impl<T: Decode> Decode for Box<[T]> {
    fn decode(d: &mut Decoder<'_>) -> Result<Box<[T]>, DecodeError> {
        d.seq().map(Vec::into_boxed_slice)
    }
}

impl<T: Decode> Decode for Box<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Box<T>, DecodeError> {
        T::decode(d).map(Box::new)
    }
}

impl<T: Decode> Decode for Arc<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Arc<T>, DecodeError> {
        T::decode(d).map(Arc::new)
    }
}

impl<T: Decode> Decode for Option<T> {
    fn decode(d: &mut Decoder<'_>) -> Result<Option<T>, DecodeError> {
        if d.bool()? {
            Ok(Some(T::decode(d)?))
        } else {
            Ok(None)
        }
    }
}

impl<A: Decode, B: Decode> Decode for (A, B) {
    fn decode(d: &mut Decoder<'_>) -> Result<(A, B), DecodeError> {
        Ok((A::decode(d)?, B::decode(d)?))
    }
}

impl<A: Decode, B: Decode, C: Decode> Decode for (A, B, C) {
    fn decode(d: &mut Decoder<'_>) -> Result<(A, B, C), DecodeError> {
        Ok((A::decode(d)?, B::decode(d)?, C::decode(d)?))
    }
}
