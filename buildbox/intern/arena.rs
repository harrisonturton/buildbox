//! Append-only storage for interned text.

use std::sync::Mutex;

/// How much text each chunk holds. Strings longer than a quarter of this get
/// an allocation of their own, so little of a chunk is wasted.
const CHUNK_BYTES: usize = 64 * 1024;

/// The length that precedes each string's bytes.
const LEN_BYTES: usize = std::mem::size_of::<u32>();

/// Stores strings for the life of the process, packed into large chunks so
/// that each costs only its bytes and a 4-byte length, not an allocation of
/// its own.
pub(crate) struct Arena {
    current: Mutex<Chunk>,
}

/// The chunk strings are being added to. Kept as a raw pointer, since the
/// parts already handed out are referred to elsewhere.
struct Chunk {
    base: *mut u8,
    capacity: usize,
    used: usize,
}

// The chunk is only reached through the arena's lock.
unsafe impl Send for Chunk {}

/// A string stored in an [`Arena`], referred to by a thin pointer: half the
/// size of a `&str`, since the length is stored with the text.
#[derive(Clone, Copy)]
pub(crate) struct Text(*const u8);

// The text is immutable and lives forever.
unsafe impl Send for Text {}
unsafe impl Sync for Text {}

impl Text {
    pub(crate) fn as_str(self) -> &'static str {
        // SAFETY: `Arena::alloc` wrote a length then that many bytes of UTF-8,
        // which are never written again or freed.
        unsafe {
            let len = u32::from_le_bytes(*(self.0 as *const [u8; LEN_BYTES])) as usize;
            let bytes = std::slice::from_raw_parts(self.0.add(LEN_BYTES), len);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    pub(crate) fn as_ptr(self) -> *const u8 {
        self.0
    }

    /// # Safety
    ///
    /// `ptr` must have come from [`Text::as_ptr`].
    pub(crate) unsafe fn from_ptr(ptr: *const u8) -> Text {
        Text(ptr)
    }
}

impl Arena {
    pub(crate) const fn new() -> Arena {
        Arena {
            current: Mutex::new(Chunk {
                base: std::ptr::null_mut(),
                capacity: 0,
                used: 0,
            }),
        }
    }

    /// Copy `text` into the arena.
    ///
    /// Panics if `text` is 4GiB or longer; interned names are never that long.
    pub(crate) fn alloc(&self, text: &str) -> Text {
        let len = u32::try_from(text.len()).expect("interned text is shorter than 4GiB");
        let record_len = LEN_BYTES + text.len();
        let record = self.reserve(record_len);
        record[..LEN_BYTES].copy_from_slice(&len.to_le_bytes());
        record[LEN_BYTES..].copy_from_slice(text.as_bytes());
        Text(record.as_ptr())
    }

    /// Hand out `len` bytes that no one else will be given.
    fn reserve(&self, len: usize) -> &'static mut [u8] {
        if len > CHUNK_BYTES / 4 {
            return Box::leak(vec![0u8; len].into_boxed_slice());
        }

        let mut chunk = self.current.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if chunk.capacity - chunk.used < len {
            let data: &'static mut [u8] = Box::leak(vec![0u8; CHUNK_BYTES].into_boxed_slice());
            *chunk = Chunk {
                base: data.as_mut_ptr(),
                capacity: data.len(),
                used: 0,
            };
        }

        let start = chunk.used;
        chunk.used += len;

        // SAFETY: each byte range is handed out once, and chunks are leaked.
        unsafe { std::slice::from_raw_parts_mut(chunk.base.add(start), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_copies_text() {
        let arena = Arena::new();
        let a = arena.alloc("hello");
        let b = arena.alloc("world");
        assert_eq!(a.as_str(), "hello");
        assert_eq!(b.as_str(), "world");
        assert_eq!(arena.alloc("").as_str(), "");
    }

    #[test]
    fn alloc_handles_text_spanning_chunks() {
        let arena = Arena::new();
        let piece = "x".repeat(CHUNK_BYTES / 5);
        let pieces: Vec<Text> = (0..20).map(|_| arena.alloc(&piece)).collect();
        assert!(pieces.iter().all(|p| p.as_str() == piece));

        let large = "y".repeat(CHUNK_BYTES * 2);
        assert_eq!(arena.alloc(&large).as_str(), large);
    }
}
