//! Keeping the server's memory use close to what it holds.
//!
//! bb allocates with jemalloc, which returns freed pages to the system from
//! a background thread soon after they're freed. Commands free in bursts,
//! so after each one the server returns them at once.

pub use jemalloc::Stats as AllocatorStats;

/// Configure the allocator for a long-lived server: return freed pages from
/// background threads. Call once, when the server starts.
pub fn configure_allocator() {
    if !jemalloc::start_background_threads() {
        tracing::debug!("jemalloc's background threads didn't start");
    }
}

/// Return freed memory to the system.
pub fn release_free_memory() {
    jemalloc::purge();
}

/// What the allocator holds now, if it's jemalloc.
pub fn allocator_stats() -> Option<AllocatorStats> {
    jemalloc::stats()
}
