# skystore: the value store

`buildbox/skystore`

skystore holds every value in the build graph, plus the traces that record
how values were computed, as compactly as it can. It knows nothing about
keys, Bazel or evaluation. It stores byte strings (and some live objects),
identified by [fingerprint](skycodec.md#fingerprints), and reference-counts
them.

## The interface

```rust
let value: ValueRef = store.insert(kind, fingerprint, &bytes); // +1 reference
store.retain(value);                                           // +1
let bytes = store.read(value)?;                                // decompressed
store.release(value);                                          // -1; frees at 0
```

- **One `ValueRef` per distinct value.** Inserting a value that's already
  stored returns the existing `ValueRef` and adds a reference. A
  `ValueRef` is a `u32` (a shard and an index), so comparing two refs
  compares two values. The evaluator relies on this for change pruning.
- **Kinds.** Each value has a `Kind`, a `u16` assigned by the layer above:
  one for each key type's values, one for each key type's traces, and one
  for each type of [part](skyframe.md#parts). Values of one kind share a
  compression dictionary and statistics.
- **Live values.** Values that can't be encoded, such as evaluated
  Starlark, are stored as `Arc<dyn Any>` with a fingerprint and an
  approximate size. They're deduplicated by fingerprint like encoded ones.
- **Reference counting.** `insert` and `retain` add a reference, and
  `release` removes one, freeing the value at zero.
  `release_and_take` returns the freed value, so the caller can release the
  references *it* held. That's how freeing a trace releases its value and
  its dependencies' values.

## How values are kept

### Entries and shards

Values are spread over 16 shards by fingerprint, each with its own lock.
A shard has a vector of 40-byte `Entry`s (fingerprint, reference count,
kind and slot) and a hash index from fingerprint to entry. Freed entries
are reused.

An entry's slot says where its bytes are:

| Slot | Used for |
|---|---|
| `Inline` | Stored bytes of 12 bytes or fewer, held in the entry itself. |
| `Paged` | Bytes packed into one of the shard's pages, in memory. |
| `Disk` | Bytes in a segment file, read through a memory mapping. |
| `Live` | A live object. |
| `Absent` | A live object a restored store knows the fingerprint of but doesn't have yet. |

### Compression with per-kind dictionaries

Each value is compressed with zstd at level 3, which is fast enough to run
as values are computed. Graph values are mostly small and much like others
of their kind, and a small value compressed on its own barely shrinks, so
the store trains a **dictionary per kind**:

1. Until a kind has a dictionary, its values are compressed without one,
   and a sample of each new value is kept (`dictionaries.rs`).
2. After 1000 samples, or 1 MiB of them, the store trains a dictionary of
   up to 32 KiB. If training fails, it tries again with twice as many
   samples, up to four times. Samples across all kinds are capped at
   8 MiB.
3. Once trained, the kind's existing values are recompressed with the
   dictionary where that makes them smaller, and new values use it.

Values under 24 bytes aren't compressed, because the frame overhead would
outweigh any saving. A value that compression doesn't shrink is stored raw.
zstd frames leave out what the store records itself (magic number,
content size, dictionary id, checksum), and the few zstd contexts are
pooled and shared between threads, because each can grow to megabytes.

### Pages and compaction

Compressed bytes are packed into pages (`pages.rs`), so a value costs its
length rather than its own heap allocation. Pages start at 16 KiB and grow
to 256 KiB as the store fills, and data larger than a quarter of a page
gets a page of its own. Freeing a value marks its bytes unused, and a page
with nothing left in it is released at once. When more than a page's
worth is unused and less than half of all page space is live, the shard
compacts: it copies live bytes into new pages and drops the old ones.

### On disk

A [checkpoint](persistence.md) writes values to **segment files**:
append-only files, written once and never modified, read back through a
read-only memory mapping. After a value is written to a segment, its slot
becomes `Disk` and its in-memory bytes are freed, so saving the store also
shrinks it: cold values are paged in by the operating system only when
read. A segment whose live data falls below half its size is rewritten
into the next checkpoint's segment and deleted.

## Reading

`read` copies the stored bytes out under the shard lock, then decompresses
outside it (or straight from the mapping, for `Disk`). Reads are
comparatively expensive, which is why [skyframe](skyframe.md) keeps a small
cache of decoded values in front of the store.

## Statistics

`stats()` reports, overall and per kind: values, raw and compressed bytes,
page bytes (allocated and live), entry and index overhead, dictionary
bytes, live-object bytes and zstd context memory. `bb server status` and
the engine metrics RPC show these, which is the main tool for seeing where
memory goes.
