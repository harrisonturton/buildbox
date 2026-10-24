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
