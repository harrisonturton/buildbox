//! jemalloc as Rust's global allocator, and what it reports.
//!
//! bb is a long-lived server whose threads allocate in bursts. glibc's
//! allocator keeps much of what they free, fragmented across its arenas;
//! jemalloc returns freed pages to the system from a background thread.

use std::alloc::{GlobalAlloc, Layout};
use std::ffi::{c_char, c_int, c_void};
use std::ptr;

/// The largest alignment `malloc` guarantees on x86_64.
const MIN_ALIGN: usize = 16;

/// `MALLOCX_ZERO`: zero the allocation.
const MALLOCX_ZERO: c_int = 0x40;

/// jemalloc's options, which it reads from its `malloc_conf` pointer:
/// return freed pages a second after they're freed; keep 8 arenas rather
/// than 4 per CPU, which spread long-lived objects thinly across partly
/// empty pages; and cache only allocations of up to 8 KiB per thread, since
/// each of the server's many threads otherwise holds megabytes. Background
/// threads,
/// which do that while threads are idle, are started by
/// [`start_background_threads`], since some processes must stay
/// single-threaded.
#[used]
#[unsafe(export_name = "_rjem_malloc_conf")]
static MALLOC_CONF: Options = Options(c"narenas:8,lg_tcache_max:13,dirty_decay_ms:1000,muzzy_decay_ms:0".as_ptr());

/// A C string's address, as a `const char *` jemalloc reads.
#[repr(transparent)]
struct Options(*const c_char);

// SAFETY: the string is static and never written.
unsafe impl Sync for Options {}

unsafe extern "C" {
    fn _rjem_malloc(size: usize) -> *mut c_void;
    fn _rjem_calloc(count: usize, size: usize) -> *mut c_void;
    fn _rjem_realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn _rjem_mallocx(size: usize, flags: c_int) -> *mut c_void;
    fn _rjem_rallocx(ptr: *mut c_void, size: usize, flags: c_int) -> *mut c_void;
    fn _rjem_sdallocx(ptr: *mut c_void, size: usize, flags: c_int);
    fn _rjem_mallctl(name: *const c_char, old: *mut c_void, old_len: *mut usize, new: *mut c_void, new_len: usize) -> c_int;
    fn _rjem_malloc_stats_print(write: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>, opaque: *mut c_void, options: *const c_char);
}

/// The global allocator.
pub struct Jemalloc;

/// The `mallocx` flags for an alignment: none if `malloc`'s alignment is
/// enough for the size.
fn align_flags(align: usize, size: usize) -> c_int {
    if align <= MIN_ALIGN && align <= size {
        return 0;
    }
    align.trailing_zeros() as c_int
}

// SAFETY: each function forwards to the jemalloc call with the same
// contract, passing the layout's alignment where malloc's isn't enough.
unsafe impl GlobalAlloc for Jemalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let flags = align_flags(layout.align(), layout.size());
        let ptr = match flags {
            0 => unsafe { _rjem_malloc(layout.size()) },
            _ => unsafe { _rjem_mallocx(layout.size(), flags) },
        };
        ptr.cast()
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let flags = align_flags(layout.align(), layout.size());
        let ptr = match flags {
            0 => unsafe { _rjem_calloc(1, layout.size()) },
            _ => unsafe { _rjem_mallocx(layout.size(), flags | MALLOCX_ZERO) },
        };
        ptr.cast()
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let flags = align_flags(layout.align(), layout.size());
        unsafe { _rjem_sdallocx(ptr.cast(), layout.size(), flags) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let flags = align_flags(layout.align(), new_size);
        let ptr = match flags {
            0 => unsafe { _rjem_realloc(ptr.cast(), new_size) },
            _ => unsafe { _rjem_rallocx(ptr.cast(), new_size, flags) },
        };
        ptr.cast()
    }
}

/// What jemalloc holds, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Allocated and not yet freed.
    pub allocated: u64,
    /// In pages that hold allocations: `allocated`, and the free space
    /// between them.
    pub active: u64,
    /// jemalloc's own bookkeeping.
    pub metadata: u64,
    /// In pages jemalloc has touched and not returned: allocations, its
    /// metadata, and freed memory not yet purged.
    pub resident: u64,
    /// Mapped from the system, resident or not.
    pub mapped: u64,
}

/// What jemalloc holds now, or `None` if it isn't the allocator in use.
pub fn stats() -> Option<Stats> {
    let mut epoch: u64 = 1;
    // SAFETY: writing `epoch` refreshes the statistics; it takes a u64.
    let refreshed = unsafe { _rjem_mallctl(c"epoch".as_ptr(), ptr::null_mut(), ptr::null_mut(), (&raw mut epoch).cast(), size_of::<u64>()) };
    if refreshed != 0 {
        return None;
    }
    let stats = Stats {
        allocated: read_size(c"stats.allocated")?,
        active: read_size(c"stats.active")?,
        metadata: read_size(c"stats.metadata")?,
        resident: read_size(c"stats.resident")?,
        mapped: read_size(c"stats.mapped")?,
    };
    (stats.mapped > 0).then_some(stats)
}

/// jemalloc's full report of what it holds, merged across arenas: by size
/// class, how many regions and pages are in use, which shows how much
/// memory is held by pages that are only partly used.
pub fn report() -> String {
    let mut report = String::new();
    // SAFETY: `append` is only called during this call, with `report` as
    // its opaque pointer; "a" omits the per-arena sections.
    unsafe {
        _rjem_malloc_stats_print(Some(append), (&raw mut report).cast(), c"a".as_ptr());
    }
    report
}

unsafe extern "C" fn append(report: *mut c_void, text: *const c_char) {
    // SAFETY: `report` is the String `report` passed, and jemalloc passes a
    // NUL-terminated string.
    let (report, text) = unsafe { (&mut *report.cast::<String>(), std::ffi::CStr::from_ptr(text)) };
    report.push_str(&text.to_string_lossy());
}

/// Start threads that return freed pages to the system in the background,
/// for a long-lived process. Returns whether they started.
pub fn start_background_threads() -> bool {
    let mut enable = true;
    // SAFETY: `background_thread` takes a bool.
    let started = unsafe { _rjem_mallctl(c"background_thread".as_ptr(), ptr::null_mut(), ptr::null_mut(), (&raw mut enable).cast(), size_of::<bool>()) };
    started == 0
}

/// Return every arena's freed pages to the system now.
pub fn purge() {
    // SAFETY: `arena.<MALLCTL_ARENAS_ALL>.purge` takes and returns nothing.
    unsafe {
        _rjem_mallctl(c"arena.4096.purge".as_ptr(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), 0);
    }
}

fn read_size(name: &std::ffi::CStr) -> Option<u64> {
    let mut value: usize = 0;
    let mut len = size_of::<usize>();
    // SAFETY: the statistics read here are all size_t.
    let read = unsafe { _rjem_mallctl(name.as_ptr(), (&raw mut value).cast(), &mut len, ptr::null_mut(), 0) };
    (read == 0).then_some(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_aligned_and_zeroed() {
        let layout = Layout::from_size_align(100, 4096).unwrap();
        // SAFETY: the layout has a non-zero size, and the block is freed
        // with it.
        unsafe {
            let ptr = Jemalloc.alloc_zeroed(layout);
            assert!(!ptr.is_null());
            assert_eq!(ptr as usize % 4096, 0);
            assert!(std::slice::from_raw_parts(ptr, 100).iter().all(|byte| *byte == 0));
            let grown = Jemalloc.realloc(ptr, layout, 10_000);
            assert_eq!(grown as usize % 4096, 0);
            Jemalloc.dealloc(grown, Layout::from_size_align(10_000, 4096).unwrap());
        }
    }

    #[test]
    fn reports_statistics() {
        let block = vec![0u8; 1 << 20];
        let stats = stats();
        drop(block);
        assert!(stats.is_some());
    }
}
