# Persistence: keeping state across restarts

A bb server keeps three kinds of state on disk, all under the server's
root directory (`<root>/bazel/`):

| What | Where | Package |
|---|---|---|
| The engine: interned names, keys, traces and values | `engine/manifest`, `engine/segments/*.seg` | `skyframe::persist`, `skystore::checkpoint` |
| Source file digests | `state/source-digests` | `persist`, `bazel/execution/source.rs` |
| Recent action results by a cheap key | `state/local-actions` | `persist`, `bazel/execution/spawn.rs` |

The CAS and action cache are separate, and persistent in their own right.

Everything here can be recomputed. If any of it is missing, damaged, or
written by another build of bb, it's ignored and the server starts cold.
Persistence never makes a build wrong; at worst it makes a build slower.

## The engine

### What's saved

The engine is saved as its **memo**: the traces it remembers for each key,
and every value those traces reach. A restored engine has no workspace
graphs. When a workspace asks for a key after the restart, the graph
*reuses* a remembered trace exactly as it would reuse another workspace's,
by verifying its dependencies. So restoring needs no special evaluation
logic: a restart looks like a new workspace arriving, and only what
changed since is recomputed.

A checkpoint writes:

- **Segments** of values (`skystore/checkpoint.rs`). Values reached by the
  memo that aren't on disk yet go into one new segment file, written once
  and never modified. From then on, they're read from it through a memory
  mapping, and their in-memory bytes are freed, so a checkpoint also
  shrinks the running server.
- **A manifest**, holding everything else: the intern tables
  ([intern](intern.md#why-ids-are-process-local-and-how-they-survive-restarts)),
  kind and part names with their ids, the key table, the memo, and the
  store's image (each value's entry at its shard and index, with each
  kind's compression dictionary and the list of segments). The manifest
  is written to a temporary file and renamed, so a crash leaves either the
  old manifest or the new one, never a torn one.

References survive because everything keeps its id. Symbols, paths, kinds,
keys and `ValueRef`s are restored at the same ids they had, so the bytes
in the segments and the traces in the memo mean the same thing in the new
process.

### Live values

Live values (evaluated `.bzl` modules, analyzed targets) can't be saved.
Their entries are saved as `Absent`, with their fingerprint and no
contents. After a restart:

- a trace whose own value is absent can't be reused, so that key is
  recomputed;
- when the key recomputes, its value has the same fingerprint as before,
  so the store gives it the **same `ValueRef`** it had;
- traces of other keys that depend on it therefore still match, and
  verify without being recomputed.

So a restart re-evaluates Starlark, but not the encoded results downstream
of it, such as action outputs.

### When it runs

The server checkpoints when no command is running (`after_command` in
`bazel/server/server.rs`) and when it stops. A checkpoint is skipped if the
memo hasn't changed since the last one. If the saved values would exceed
2 GiB, the checkpoint keeps only each key's newest trace. A segment whose
live data falls below half its size is rewritten into the next segment
and deleted. Traces that refer to a value the checkpoint didn't save,
because it was freed meanwhile, are dropped from the saved memo, since its
ref could be reused for another value in the restored store.

### Validity

The manifest starts with a magic number, a format version and the
**build identity**: a fingerprint of bb's own executable, computed at
startup. A manifest from any other build is ignored, and its segments
deleted, because interned ids, kind numbering and encodings may mean
something else in a different build. The manifest and each segment carry
checksums, and a damaged one is ignored the same way.

`Engine::open` must run before anything else is interned or any key type
is registered, so that the saved ids are free to restore. The server opens
the engine first thing in `BazelServer::new`.

## Small persisted maps

`persist::PersistedMap` is a bounded map from fingerprints to small values,
saved to one file: written to a temporary file and renamed, with a
checksum. It holds at most a set number of entries, forgetting those used
longest ago. A file that can't be read starts the map empty.

Two are used:

- **Source digests** (up to 500,000 entries). A source file's digest is
  keyed by its path and what `stat` says of it (device, inode, size, mode,
  modification and change times), so an unchanged file isn't read and
  hashed again after a restart. A digest is saved only once the file has
  been unchanged for two seconds, so an edit within the timestamps'
  resolution can't be mistaken for no change.
- **Local action results** (up to 200,000 entries). A spawn's result is
  remembered by a fingerprint of its command line and every input's path
  and digest. That's cheaper to compute than the action's REAPI digest,
  which needs its whole input Merkle tree, so a previously run action is
  found without building one. The result is used only if its outputs are
  still in the CAS. Progress output reports these hits as Bazel reports
  its own action cache hits.

Both are saved after every command.
