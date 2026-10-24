# skycodec: compact encoding and fingerprints

`buildbox/skycodec`

skycodec is the binary format that every key and most values in the build
graph are written in, and the fingerprint that identifies them. It's
designed for one thing: making many small, similar records as small as
possible, cheaply.

## The encoding

- **No field names, no tags, no schema.** A type's `Encode` and `Decode`
  impls write and read its fields in a fixed order. A struct is its fields
  back to back, and an enum is its variant index followed by that
  variant's fields.
- **Variable-length integers.** Unsigned integers are LEB128 (seven bits a
  byte), and signed integers are zigzag-encoded first, so small
  magnitudes stay small. Most lengths, counts and ids fit in one byte.
- **Interned values as ids.** A `Symbol` or `PathId` is written as its
  [interned](intern.md) `u32`, so a label is a few bytes, not its text.
- **Lengths before variable data.** Strings, byte strings and lists are
  preceded by their length.

`#[derive(Encode, Decode)]` (in `skycodec/derive`) implements both for
structs and enums whose fields implement them, so most graph types just
derive them:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ConfiguredTargetKey {
    pub label: Label,     // three interned ids
    pub config: ConfigId, // one more
}
```

`to_bytes` encodes into a new buffer. `from_bytes` decodes, and fails if
any bytes are left over.

### What the encoding gives up

Because the format has no schema, changing a type's fields or variants
changes its encoding, and old bytes no longer decode correctly. Because
it writes interned ids, an encoding is only meaningful within the process
that wrote it. Both are fine because encodings never leave the process,
with one exception: the saved engine, which is tied to the exact build of
bb that wrote it and is thrown away by any other ([Persistence](persistence.md)).

### Decoding is defensive

Data the process encoded itself always decodes, so a decode error means
corruption or a type mismatch. Decoding never panics on bad input. It
returns a `DecodeError` saying what went wrong and where: unexpected end,
integer overflow, invalid UTF-8, an unknown interned id, an invalid enum
variant, or trailing bytes. Code that reads lengths from data caps
allocations so a corrupt length can't exhaust memory.

## Fingerprints

A `Fingerprint` is the first 128 bits of a SHA-256 hash. That's enough to
make accidental collisions practically impossible among the values one
machine produces, at half the size of a full digest. Full digests are
still used wherever the remote execution protocol requires them.

A `Fingerprinter` hashes a sequence of parts, writing each part's length
before its bytes, so different ways of splitting the same bytes give
different fingerprints.

Fingerprints are used to:

- **identify stored values.** The [store](skystore.md) deduplicates by
  fingerprint. A value's fingerprint covers its kind and its encoding, so
  equal bytes of two different kinds are different values.
- **identify live values.** Values that can't be encoded, such as
  evaluated Starlark modules, carry a fingerprint of what they were
  computed from (source, repository mapping, and the fingerprints of
  their loads), and two with the same fingerprint are treated as equal.
- **check files on disk.** The saved engine's manifest and each segment
  file carry a fingerprint checksum.
