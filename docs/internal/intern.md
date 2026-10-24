# intern: 4-byte ids for names and paths

`buildbox/intern`

The build graph mentions the same names constantly: repository names,
package paths, target names, path segments. `intern` stores each distinct
one once, for the life of the process, and hands out a dense `u32` id for
it. Ids compare and hash as integers, and the text is shared by every
workspace that uses it. Nothing interned is ever freed.

This is the bottom layer. Everything above it, from labels to stored
values, is built from these ids, which is why labels are 12 bytes and why
encoded values are small.

## What's interned

| Type | What it is | Size |
|---|---|---|
| `Symbol` | A string: a target name, a repository name, a path segment. | 4 bytes |
| `PathId` | A normalized relative path, such as a package path. | 4 bytes |
| `BlobTable` | A table of byte strings, each given an id. Used for keys and configurations. | 4-byte ids |

**Paths are a tree.** A `PathId` is its parent's id and its last segment's
`Symbol`, packed into 64 bits. Every path costs 8 bytes however long it
is, and paths with a common prefix share it: `a/b/c` and `a/b/d` share
`a/b`. The empty path is `PathId::ROOT`. Paths order as their text does.

## How it works

- **`Table`** (`table.rs`) assigns ids. Each entry is a 64-bit value: a
  pointer to text, or a packed parent and name. Lookup goes through 64
  independently locked shards, so threads interning different values
  rarely contend. The index stores only ids, hashing and comparing through
  the values, at about five bytes an entry.
- **`SegVec`** (`segvec.rs`) maps ids to values. It's a list of buckets
  that double in size, so existing entries never move, and an id's value
  can be read without a lock once it's been handed out.
- **`Arena`** (`arena.rs`) holds the text: strings are packed into 64 KiB
  chunks, each preceded by a 4-byte length, so a string costs its bytes
  plus four, not a heap allocation of its own. A reference to the text is
  a thin pointer.

## Why ids are process-local, and how they survive restarts

Ids are handed out in the order values are first interned, so the same
string can have different ids in two processes. Anything that writes ids
down, such as [skycodec](skycodec.md) encodings, is only meaningful in the
process that wrote it.

To keep the engine across restarts, [persistence](persistence.md) saves the
intern tables in id order (`snapshot.rs`) and restores them by interning
the entries again, in the same order, before anything else is interned.
Because ids are dense and sequential, each entry gets back exactly the id
it had.

That's why `Engine::open` must run before any other interning or key
registration: the restored ids have to be free.
