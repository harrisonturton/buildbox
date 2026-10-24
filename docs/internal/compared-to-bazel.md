# bb compared with Bazel

bb is meant to be a drop-in replacement for Bazel: same commands, flags,
output and exit codes. Underneath, it's a different design. This page
explains what problems with Bazel motivated bb, how bb's architecture
addresses each one, and what it costs.

Statements about Bazel here were checked against Bazel's source at the
version bb targets (9.3.1, and Bazel's `master` as of October 2026). File
references are to that source tree.

## Architectural goals

1. **One server per machine.** Every workspace and worktree shares one
   process, and commands never wait for each other.
2. **Memory proportional to distinct work, not to the number of
   checkouts.** Ten worktrees of one repository should cost little more
   than one.
3. **Build state that survives.** Loading and analysis results outlive a
   server restart, a change of flags, and a switch between worktrees.
4. **A small, predictable footprint.** No garbage-collected heap sized for
   the worst case, memory returned to the system when idle, and every
   value held compactly.
5. **Indistinguishable from Bazel from the outside,** except for being
   more responsive.

## The problems, and what bb does instead

### 1. A new server for every workspace

**Bazel.** The Bazel client starts one server per *output base*, and by
default the output base is a hash of the workspace's path:
`output_base = GetHashedBaseDir(output_user_root, workspace)`
(`src/main/cpp/startup_options.cc`, an MD5 digest in
`blaze_util_posix.cc`). Each repository, and each git worktree of the same
repository, gets its own output base and therefore its own JVM, with its
own copy of every loaded package, analyzed target and action. Ten agents
working in ten worktrees means ten Bazel servers loading and analyzing
nearly identical code ten times.

Within one output base, commands are serialized: the server takes a
command lock, and a second command prints *"Another command (...) is
running. Waiting for it to complete on the server"*
(`BlazeCommandDispatcher.java`). You can't run a query while a build runs.

**bb.** There's one server, `bb up`, for every workspace on the machine.
Each workspace gets its own [graph](skyframe.md), but all graphs share one
engine, which holds each distinct value once. When worktree B needs a key
that worktree A computed, the engine checks whether A's
[trace](skyframe.md#traces) for that key is still valid in B, meaning each
dependency has the same value in B, and if so reuses the value without
computing anything. Keys don't contain the workspace's absolute path (file
keys use a `Root::Workspace` placeholder that each graph resolves), so
worktrees at different paths have identical keys.

Because the server is shared, `bb shutdown` doesn't stop it. It frees
only the calling workspace's graph, and every other workspace keeps
working.

There's no server lock. Commands run concurrently, in the same workspace
or different ones. Two commands that need the same key share one
computation, because a key requested while it's in flight waits for that
computation rather than starting another.

External repositories are fetched once per machine, into a directory named
by what defines them (`bazel/bzlmod/fetch_repo.rs`), and downloads are
cached by SHA-256, so a second worktree doesn't fetch or extract anything.

### 2. A large JVM heap

**Bazel.** The server is a Java program. The client doesn't set a maximum
heap (`-Xmx`) unless you pass one with `--host_jvm_args`
(`src/main/cpp/blaze.cc`, `startup_options.cc`), so the JVM's default
applies: a quarter of physical memory. Everything Bazel knows lives as Java
objects on that heap, each with an object header, references and padding.
A garbage-collected heap also needs room to spare: the JVM grows the heap
to keep collections infrequent, and by default gives little of it back to
the operating system, so an idle server stays close to its peak footprint
(2.2 GB idle against a 2.5 GB peak in the benchmark below).
Bazel offers ways to trade speed for memory (`--discard_analysis_cache`,
`--nokeep_state_after_build`, Skyframe's memory-saving flags), and each
throws away incremental state.

**bb.** bb is native Rust. It allocates with jemalloc and returns freed
memory to the system after every command (`bazel/server/memory.rs`). Most
of what it knows isn't held as objects at all:

- **Interned ids.** Every name, path segment and path is interned once per
  process as a 4-byte id ([intern](intern.md)). A label is 12 bytes, a
  configuration reference 4, and a path 8 bytes however long, sharing its
  prefix with its siblings.
- **Encoded and compressed values.** Graph values are encoded with
  [skycodec](skycodec.md), which has no field names or tags, and integers
  are variable-length. [skystore](skystore.md) compresses each value with
  zstd, using a dictionary trained per kind of value, because individual
  values are too small to compress well alone. Values are decoded only when
  read, through a small cache (16 MiB by default).
- **Deduplication.** Identical values are stored once and referred to by a
  4-byte `ValueRef`, however many keys, workspaces or traces hold them.
  Comparing two values for change pruning compares two integers.
- **Shared structure.** Action inputs are kept as `InputSet`s, Bazel's
  nested sets, with equal subsets shared across every action and target
  (`bazel/analysis/inputs.rs`), and the encodable bulk of each action is
  stored compressed in the engine as a `Stored` part.
- **Small graph nodes.** A key's entry in a workspace's graph is 16 bytes.
  The keys themselves are interned in the engine once.

### 3. Heaps grow with the number of checkouts

This is the multiplication of the first two problems. Each Bazel server's
heap holds its own copy of the packages, `.bzl` modules, configured targets
and actions of its workspace, and nothing is shared between servers. With
several worktrees of one large repository, the machine holds several
near-identical copies, each of several gigabytes. Running out of memory
with many concurrent Bazel servers is what started bb.

**bb.** Memory scales with the *distinct* values all workspaces have, not
with the number of workspaces. An unchanged `.bzl` file, toolchain or
package is one value in the store whichever worktree computed it, and even
live Starlark values (evaluated modules and analyzed targets) are
deduplicated by a fingerprint of what they were computed from. Each
additional worktree costs its graph's 16-byte nodes plus the values it
really has differently.

As a snapshot, on one benchmark (a medium-sized repository's backend,
`-c opt`, measured on the `bazel-compat` branch on 2026-10-24):

| | Bazel 8.8.1 | bb |
|---|---|---|
| Cold build | 149.7 s | 149.8 s |
| Peak memory | 2.5 GB | 814 MB |
| Idle memory after the build | 2.2 GB | 580 MB |
| First build after a server restart | 6.5 s | 1.8 s |
| No-op build | 0.46 s | 0.12 s |
| Rebuild after editing one file | 0.91 s | 1.46 s |

These are per workspace. A second worktree adds a second 2.2 GB Bazel
server, while bb's footprint barely moves. The numbers will drift as both
change. The one-file edit was slower in bb at the time, and is a known gap.

### 4. Query and analysis state isn't kept, shared or remote

**Bazel.** Loaded packages and analyzed targets exist only in the server's
heap, in that server's Skyframe graph. That state is lost when:

- **the server stops**, which happens on `bazel shutdown`, after three
  idle hours by default (`max_idle_secs = 3 * 3600` in
  `startup_options.cc`), when the client needs different startup options,
  and when the JVM runs out of memory;
- **build options change.** Switching between, say, `-c opt` and
  `-c fastbuild` prints *"... discarding analysis cache (this can be
  expensive ...)"* and re-analyzes from scratch
  (`SkyframeBuildView.shouldDiscardAnalysisCache`);
- **you move to another worktree.** Its server has its own graph, starting
  cold.

Remote execution doesn't help. The Remote Execution API caches action
results and file blobs, not packages, configured targets or query results,
so a fresh server with a warm remote cache still loads and analyzes
everything before it can find cache hits. Bazel has experimental work on
caching analysis remotely ("Skycache", the
`--experimental_remote_analysis_cache_mode` flag in
`RemoteAnalysisCachingOptions.java`). In open-source Bazel it's
undocumented, off by default, ships only an in-memory store
(`SerializationModule.java`), and isn't part of REAPI, so a standard
remote cache can't serve it.

**bb.** Loading and analysis are graph keys like everything else, and the
engine keeps their results in three ways:

- **Across configurations.** A configured target's key includes its
  4-byte `ConfigId`, and configurations are interned, not replaced, so
  targets analyzed with `-c opt` and with `-c fastbuild` live side by side.
  Switching back finds the earlier analysis still valid. Nothing is
  discarded.
- **Across workspaces.** Any workspace reuses another's results through
  traces, as described above. A query in a fresh worktree reads packages
  another worktree already loaded.
- **Across restarts.** When no command is running, the server checkpoints
  the engine to disk: the intern tables, keys, remembered traces and
  values, in append-only segment files with a manifest replaced atomically
  ([Persistence](persistence.md)). A restarted server restores it and
  reuses its traces like another workspace's. Only what changed is
  recomputed, plus live Starlark values, which can't be saved and are
  recomputed on demand. These recompute to the same fingerprint, and so
  get their old `ValueRef` back, so traces that depend on them stay valid.
  Source file digests and recent action results are kept across restarts
  too (`persist::PersistedMap`).

bb doesn't put analysis state in a *remote* cache either. Its answer is
that one long-lived server per machine, persisted to local disk, makes a
remote analysis cache unnecessary for the problem bb targets: many
checkouts on one machine. Actions still go through a REAPI backend, which
can be bb's own in-process executor or a remote execution service, so
action results are shared through the action cache as usual.

## Other architectural changes

### Async key functions, no restarts

A Bazel `SkyFunction` asks for its dependencies through an environment.
If one isn't computed yet, the function returns `null` and is restarted
from the top once it is (unless it keeps state between attempts in a
`SkyKeyComputeState`). bb's key functions are Rust futures: `ctx.compute(&key).await`
suspends the function until the dependency is ready, then continues where
it left off. Nothing is computed twice. Dependencies asked for together
(`ctx.compute_many`) are computed in parallel, and the grouping is
recorded so that later checks verify them in parallel too.

### Verifying traces, shared between workspaces

Bazel's Skyframe checks a dirty node by comparing versions of its
dependencies within one graph. bb records, for each computation, a
[trace](skyframe.md#traces): the dependencies read, in order and in
groups, with the values they had. A trace is valid anywhere its
dependencies have the same values, so the engine can hand one
workspace's result to another, or to itself after a restart. The engine
keeps up to four traces per key, so workspaces at different revisions of
a repository can each find a match.

### Analysis and execution in one graph

Actions are keys (`ACTION_EXECUTION`) in the same graph as packages and
targets. Their value is only the digests of their outputs, since the bytes
are in the CAS. An action that is unchanged in another worktree isn't
re-run or even looked up again. Execution starts as soon as each
top-level target is analyzed. See [Skymeld](skymeld.md).

### Execution always goes through REAPI

bb doesn't have a separate local execution strategy. Every action runs
through a `Backend` of a CAS, an action cache and an executor: either the
server's own (sandboxed or containerized, on this machine) or a remote
service's. Local and remote builds share one code path and one cache
format. A cheap persisted key (`LocalActionCache`) finds a previous result
without building the action's input Merkle tree, and a semaphore bounds
how many actions have input trees in memory at once.

### Outputs are written only when they change

bb remembers what each build wrote to a workspace's output tree, with
each file's identity (`bazel/server/written.rs`), and writes only outputs
that changed or were touched since, as Bazel leaves unchanged outputs in
place.

## What it costs

- **Fidelity is earned feature by feature.** bb reimplements Bazel's
  semantics, so every builtin, flag and output format has to be written
  and checked against real Bazel. bb supports only the flags it
  implements, and unsupported `cc_internal` functions fail with an error
  saying bb doesn't support them yet.
- **Live Starlark values can't be saved.** Evaluated `.bzl` modules and
  analyzed targets are recomputed after a restart, which is most of the
  first build's cost after a restart.
- **The encoding is private to one build of bb.** Interned ids and
  encodings mean something only to the program that wrote them. A saved
  engine is ignored by any other build of bb, so every upgrade starts with
  a cold engine. The CAS and action cache are unaffected.
- **Sharing makes failures shared.** A bug in one key's function can now
  give wrong results to every workspace. That's why the contract that a
  key's value depends only on its key and the values it reads is enforced
  by the design, and why external keys (those that read the disk or
  environment) are never shared.
