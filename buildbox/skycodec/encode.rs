//! Writing the encoding.

use crate::Fingerprint;
use intern::{PathId, Symbol};
use std::sync::Arc;

/// Appends encoded values to a buffer.
#[derive(Default)]
pub struct Encoder {
    buf: Vec<u8>,
}

/// A type that can be written with an [`Encoder`].
pub trait Encode {
    fn encode(&self, e: &mut Encoder);
}

impl Encoder {
    pub fn new() -> Encoder {
        Encoder::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// An unsigned integer as LEB128: seven bits a byte, low bits first.
    pub fn u64(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.buf.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.buf.push(value as u8);
    }

    pub fn u32(&mut self, value: u32) {
        self.u64(value as u64);
    }

    pub fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }

    /// A signed integer, zigzag-encoded so small magnitudes stay small.
    pub fn i64(&mut self, value: i64) {
        self.u64(((value << 1) ^ (value >> 63)) as u64);
    }

    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub fn bool(&mut self, value: bool) {
        self.buf.push(value as u8);
    }

    /// Bytes, preceded by their length.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.usize(bytes.len());
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes with no length, for fixed-size data.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    pub fn symbol(&mut self, symbol: Symbol) {
        self.u32(symbol.id());
    }

    pub fn path(&mut self, path: PathId) {
        self.u32(path.id());
    }

    /// Increasing `u32`s, as the gaps between them. A list of nearby ids,
    /// such as a node's dependencies, takes about a byte each.
    pub fn sorted_u32s(&mut self, values: &[u32]) {
        self.usize(values.len());
        let mut previous = 0u32;
        for (i, &value) in values.iter().enumerate() {
            debug_assert!(i == 0 || value > previous, "values must be strictly increasing");
            self.u32(value - previous);
            previous = value;
        }
    }

    /// A sequence of values, preceded by its length.
    pub fn seq<T: Encode>(&mut self, values: &[T]) {
        self.usize(values.len());
        for value in values {
            value.encode(self);
        }
    }
}

macro_rules! encode_uint {
    ($($ty:ty),*) => {$(
        impl Encode for $ty {
            fn encode(&self, e: &mut Encoder) {
                e.u64(*self as u64);
            }
        }
    )*};
}

encode_uint!(u16, u32, u64, usize);

impl Encode for u8 {
    fn encode(&self, e: &mut Encoder) {
        e.u8(*self);
    }
}

impl Encode for i64 {
    fn encode(&self, e: &mut Encoder) {
        e.i64(*self);
    }
}

impl Encode for i32 {
    fn encode(&self, e: &mut Encoder) {
        e.i64(*self as i64);
    }
}

impl Encode for bool {
    fn encode(&self, e: &mut Encoder) {
        e.bool(*self);
    }
}

impl Encode for str {
    fn encode(&self, e: &mut Encoder) {
        e.str(self);
    }
}

impl Encode for String {
    fn encode(&self, e: &mut Encoder) {
        e.str(self);
    }
}

impl Encode for Symbol {
    fn encode(&self, e: &mut Encoder) {
        e.symbol(*self);
    }
}

impl Encode for PathId {
    fn encode(&self, e: &mut Encoder) {
        e.path(*self);
    }
}

impl Encode for Fingerprint {
    fn encode(&self, e: &mut Encoder) {
        e.raw(self.as_bytes());
    }
}

impl<T: Encode> Encode for [T] {
    fn encode(&self, e: &mut Encoder) {
        e.seq(self);
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn encode(&self, e: &mut Encoder) {
        e.seq(self);
    }
}

impl<T: Encode + ?Sized> Encode for Box<T> {
    fn encode(&self, e: &mut Encoder) {
        (**self).encode(e);
    }
}

impl<T: Encode + ?Sized> Encode for Arc<T> {
    fn encode(&self, e: &mut Encoder) {
        (**self).encode(e);
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(&self, e: &mut Encoder) {
        match self {
            None => e.bool(false),
            Some(value) => {
                e.bool(true);
                value.encode(e);
            }
        }
    }
}

impl<A: Encode, B: Encode> Encode for (A, B) {
    fn encode(&self, e: &mut Encoder) {
        self.0.encode(e);
        self.1.encode(e);
    }
}

impl<A: Encode, B: Encode, C: Encode> Encode for (A, B, C) {
    fn encode(&self, e: &mut Encoder) {
        self.0.encode(e);
        self.1.encode(e);
        self.2.encode(e);
    }
}
