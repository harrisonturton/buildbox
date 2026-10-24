//! Keeping the server's memory use close to what it holds.
//!
//! Loading allocates in bursts from many threads, which leaves glibc's
//! allocator holding memory it has freed. The server limits how many arenas
//! glibc keeps, and hands free memory back to the system after each
//! command.

/// How many malloc arenas glibc may create. Each thread otherwise gets
/// arenas of its own, which hold on to what it freed.
const MALLOC_ARENAS: i32 = 4;

/// Configure the allocator. Call once, before starting threads.
pub fn configure_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only changes the allocator's tuning.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, MALLOC_ARENAS);
    }
}

/// Return freed memory to the system.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases memory the allocator holds free.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// What the allocator holds, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllocatorStats {
    /// Allocated and not yet freed, in arenas or mapped on their own.
    pub in_use: u64,
    /// Freed but still held in arenas.
    pub free: u64,
    /// Taken from the system for arenas and large allocations.
    pub held: u64,
    pub arenas: u64,
}

/// What the allocator holds now, if it can say. The counters glibc gives
/// are 32 bits, so they wrap beyond 4 GiB.
pub fn allocator_stats() -> Option<AllocatorStats> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallinfo only reads the allocator's counters.
        #[allow(deprecated)]
        let info = unsafe { libc::mallinfo() };
        let bytes = |count: libc::c_int| count as u32 as u64;
        return Some(AllocatorStats {
            in_use: bytes(info.uordblks) + bytes(info.hblkhd),
            free: bytes(info.fordblks),
            held: bytes(info.arena) + bytes(info.hblkhd),
            arenas: bytes(info.arena),
        });
    }
    #[allow(unreachable_code)]
    None
}
