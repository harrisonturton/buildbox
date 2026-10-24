# Overview

## The process model

bb is one binary that plays two roles.

- **The client** is what you run: `bb build //...`, or `bazel build //...`
  when bb is installed under that name. It's thin. It finds the server,
  sends it the command line, working directory, environment and terminal
  size, and prints what comes back. It exits with the command's exit code.
- **The server** is `bb up`, a long-lived daemon. It's both a remote
  execution (REAPI) server, with its own CAS, action cache and sandboxed
  executor, and the Bazel server that loads, analyzes and builds. The
  client starts it in the background if it isn't running.

There is **one server per machine**, not one per workspace. Every workspace
and every worktree on the machine talks to the same process, and commands
run concurrently, in the same workspace or in different ones. There's no
server lock.

```
  bb build //...   (worktree A)  ─┐
  bb test //foo/...(worktree B)  ─┼─►  bb up
  bb query ...     (worktree A)  ─┘     ├─ Bazel service: one graph per workspace,
                                        │    all sharing one engine
                                        └─ REAPI: CAS, action cache, executor
```

The client and server speak Connect RPC (`buildbox.bazel.v1.Bazel/Run`):
the client sends a `RunRequest` and the server streams back output and,
last, the exit code. The client also sends its GNU build ID. A server that
was built from different code refuses the command; the client then asks
it to drain and starts a new one. You never talk to a server from another
build.

## The layers

Each layer is a Bazel package that depends only on the layers beneath it.
The build graph enforces this.

```
 ┌───────────────────────────────────────────────────────────────────────┐
 │ cli                 the bb binary: client, `bb up`, `bb server ...`  │
 ├───────────────────────────────────────────────────────────────────────┤
 │ bazel/server        commands, workspaces, output, the RPC service     │
 │ bazel/ui            Bazel's terminal output: messages, progress bar   │
 │ bazel/progress      what evaluations are doing, as they do it         │
 ├───────────────────────────────────────────────────────────────────────┤
 │ bazel/query         the query language                               │
 │ bazel/execution     actions as graph keys, run through a REAPI backend│
 │ bazel/analysis      configured targets, rules, providers, actions     │
 │ bazel/bzlmod        module resolution, extensions, repository rules   │
 │ bazel/loading       BUILD and .bzl files into packages                │
 │ bazel/starlark      Bazel's Starlark dialect and builtins             │
 │ bazel/packages      packages and targets as plain data                │
 │ bazel/fs            the filesystem as graph keys; change detection    │
 │ bazel/config        build configurations                              │
 │ bazel/label         labels, packages, repositories                    │
 │ bazel/options       command lines and .bazelrc files                  │
 │ bazel/fetch         downloads, archives, patches                      │
 ├───────────────────────────────────────────────────────────────────────┤
 │ skyframe            the incremental evaluator, shared by workspaces   │
 │ skystore            compressed, deduplicated value store              │
 │ skycodec            compact binary encoding and fingerprints          │
 │ persist             small maps kept across restarts                   │
 │ intern              4-byte ids for strings and paths                  │
 └───────────────────────────────────────────────────────────────────────┘
```

The bottom four packages know nothing about Bazel. They're a general
engine for incremental, memoized computation whose results are shared
between independent users and kept compactly. Everything Bazel-specific is
a set of key types on top of it.

Execution is behind a `Backend` (`bazel/execution/backend.rs`): a CAS, an
action cache and an executor, each a trait. The server wires in either its
own in-process implementations or clients for a remote execution service
(`buildbox/server/server.rs`). Analysis and loading don't know which.

## What happens when you run `bb build //foo/...`

1. **The client** finds or starts the server and sends it the command.
2. **The server** finds the workspace (the nearest directory with a
   `MODULE.bazel` or `WORKSPACE`), and gets that workspace's graph,
   creating it on first use. Every graph shares the server's engine.
3. **Refresh.** The workspace re-stats every file its graph has read, in
   parallel, and invalidates the keys of those that changed. Results that
   were transient, such as failed actions, are invalidated too. This
   starts a new revision of the graph.
4. **Options.** `.bazelrc` files and the command line are parsed into
   options, and from them a build configuration, interned as a 4-byte
   `ConfigId`.
5. **Loading.** Target patterns are resolved by evaluating `PACKAGE` keys
   (and `PACKAGES_UNDER_DIRECTORY` for `/...`). A package's key loads its
   `.bzl` files through `BZL_LOAD` keys and globs through `GLOB` keys, and
   these read files through `FILE_STATE`, `DIRECTORY_LISTING` and
   `FILE_CONTENT` keys.
6. **Analysis.** Each top-level target is a `CONFIGURED_TARGET` key. It
   analyzes its dependencies (more `CONFIGURED_TARGET` keys), resolves
   toolchains, then runs the rule's Starlark implementation on a blocking
   thread. Its value is the target's providers and the actions it
   registered.
7. **Execution starts early.** As soon as one top-level target is
   analyzed, its outputs start building while other targets are still
   being analyzed (see [Skymeld](skymeld.md)). Each action is an
   `ACTION_EXECUTION` key, whose value is just the digests of its outputs.
   It goes to the action cache first, and runs only on a miss.
8. **Outputs.** Built files are written from the CAS into the output tree
   (`bazel-out/...`), skipping those already there as they were written,
   and the convenience symlinks (`bazel-bin` and friends) are made.
9. **After the command,** the server saves small state maps, and when no
   command is running, checkpoints the engine to disk and returns freed
   memory to the operating system.

Every step from 5 onwards is evaluation of keys in the graph. A key whose
dependencies have the same values as last time isn't computed again, and a
key some other workspace already computed with the same dependency values
is reused from that workspace's trace. A no-op build checks the graph and
finds nothing to do.

## Where to go next

- [bb compared with Bazel](compared-to-bazel.md) explains the design
  choices above and what they buy.
- [skyframe](skyframe.md) explains how keys are evaluated, checked and
  shared.
- [The Bazel layers](bazel-layers.md) lists every key type and which layer
  owns it.
