# bb internals

These documents describe how `bb`, buildbox's Bazel-compatible build tool,
is put together: the layers it is made of, what each one is for, and why it
is built differently from Bazel. They're for people working on bb, not for
people using it.

bb aims to behave exactly like the `bazel` binary (currently Bazel 9.3.1)
from the outside: the same commands, flags, output and exit codes. Inside it
is a different program. The differences are what make it use a fraction of
Bazel's memory and serve many workspaces from one process, and they're what
these documents are mostly about.

## Reading order

1. [Overview](overview.md): the process model, the layers, and what
   happens when you run `bb build`.
2. [bb compared with Bazel](compared-to-bazel.md): what we changed and
   why it's faster and smaller.
3. The engine, bottom up:
   - [intern](intern.md): 4-byte ids for strings and paths.
   - [skycodec](skycodec.md): the compact binary encoding, and
     fingerprints.
   - [skystore](skystore.md): the compressed, deduplicated value store.
   - [skyframe](skyframe.md): the incremental evaluator, shared between
     workspaces.
   - [Persistence](persistence.md): keeping the engine across restarts.
4. [Skymeld](skymeld.md): how analysis and execution overlap.
5. [The Bazel layers](bazel-layers.md): labels, files, loading, Bzlmod,
   analysis, execution and the server, on top of the engine.

## Where the code is

| Directory | What it is |
|---|---|
| `buildbox/intern` | Interned strings, paths and byte strings. |
| `buildbox/skycodec` | The binary encoding and fingerprints. |
| `buildbox/skystore` | The value store. |
| `buildbox/skyframe` | The evaluator and its persistence. |
| `buildbox/persist` | Small maps kept across restarts. |
| `buildbox/bazel/*` | Bazel's semantics, one package per layer. |
| `buildbox/cli` | The `bb` binary: the client, and `bb up`. |
| `buildbox/server` | Wiring the server together, including where actions run. |
| `buildbox/{cas,actioncache,executor,scheduler,remote,...}` | The remote execution server bb builds through. |

## Vocabulary

A few words mean specific things throughout.

- **Workspace**: a directory with a `MODULE.bazel` (or `WORKSPACE`) file.
  Each git worktree is its own workspace.
- **Engine**: the one shared store of values and traces in a bb server.
- **Graph**: one workspace's view of the engine: which value each key has
  in that workspace right now.
- **Key**: a node of the build graph, such as "the package `//foo`" or "the
  third action of `//foo:bar` in configuration 7". A key knows how to
  compute its value.
- **Value**: what a key computes. Most values are encoded and compressed;
  some (evaluated Starlark) are kept as live objects.
- **Trace**: a record of one computation of a key: the dependencies it
  read, in order, with the values they had, and the value it produced.
- **Revision**: a generation of a graph. Each change to the files a graph
  read starts a new revision.
- **Kind**: the type of a key, such as `PACKAGE` or `CONFIGURED_TARGET`.
  The store keeps statistics and a compression dictionary per kind.
