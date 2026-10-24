//! Files mapped into memory, read-only.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::ptr::NonNull;

/// A whole file, mapped read-only. Its pages are the kernel's page cache:
/// read on demand, and reclaimable under memory pressure, since they can be
/// read again.
pub(crate) struct Mmap {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the mapping is read-only and lives until the `Mmap` is dropped.
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    /// Map `file`, which must not be changed while it's mapped.
    pub(crate) fn map(file: &File) -> io::Result<Mmap> {
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Mmap { ptr: NonNull::dangling(), len });
        }
        // SAFETY: a fresh, private, read-only mapping of an open file.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Mmap { ptr, len })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping covers `len` readable bytes until dropped.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: unmapping the mapping this value made.
            unsafe {
                libc::munmap(self.ptr.as_ptr().cast(), self.len);
            }
        }
    }
}
