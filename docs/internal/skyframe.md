# skyframe: the incremental evaluator

`buildbox/skyframe`

skyframe is bb's build graph: an incremental, memoizing evaluator for
async functions, whose results are shared between workspaces. It's named
after Bazel's Skyframe and plays the same role, but it's built differently
in three ways that matter:

1. **One engine, many graphs.** Values and traces live in a shared
   `Engine`. Each workspace has a `Graph` that only records which value
   each key has in that workspace.
2. **Traces, not versions.** A computation is remembered as a trace of what
   it read. A trace can be reused by any workspace where those reads give
   the same values, including a workspace that has never computed the key.
3. **Async functions.** A key function awaits its dependencies instead of
   returning early and being restarted.

## Keys and values

A key implements `Key`:

```rust
impl Key for PackageKey {
    type Value = LoadedPackage;
    const KIND: &'static str = "PACKAGE";

    async fn compute(&self, ctx: &Ctx) -> Result<LoadedPackage, SkyError> {
        let lookup = ctx.compute(&PackageLookupKey(self.0)).await?;
        // ...
    }
}
```

The contract: **a key's value is a function of the key and of the values
it reads through `ctx`, and nothing else.** That's what makes sharing
safe. The exception is keys that read the world outside the graph, such as
a file's state or the client's environment. These declare
`const EXTERNAL: bool = true`. External keys are never shared between
workspaces, are never reused from another graph's trace, and are recomputed
when [invalidated](#revisions-and-invalidation).

Keys are encoded with [skycodec](skycodec.md) and interned in the engine's
`KeyTable` as their kind followed by their encoding, giving each a `KeyId`
(`u32`). The same key has the same id in every workspace.

A value implements `Value`, which says how it's stored:

- `Storage::Encoded(bytes)` for ordinary values, which is most of them.
  `encoded_value!(T)` implements this for any `Encode + Decode` type.
- `Storage::Live { fingerprint, size }` for values that can't be encoded,
  such as an evaluated `.bzl` module or an analyzed target holding
  Starlark objects. The fingerprint must identify the value: two live
  values with the same fingerprint are treated as equal.

### Kinds

Each key type is registered on first use (`kinds.rs`) and gets a `KindId`
in the order types are first seen. The registry maps a kind to its name
and to type-erased functions that decode a key and compute it, so the
graph can compute a key from its interned bytes alone. Each kind gets two
store kinds, one for values and one for traces, so each has its own
compression dictionary and statistics.

## Traces

A `Trace` records one computation of a key:

```rust
struct Trace {
    key: KeyId,
    flags: u8,              // ERROR, TRANSIENT
    value: ValueRef,        // what it produced
    deps: Vec<Dep>,         // (KeyId, ValueRef) for each key it read
    group_ends: Vec<u32>,   // where each group of deps ends
}
```

Dependencies are recorded in **groups**: each call to `ctx.compute` or
`ctx.compute_many` is one group, in the order the function made them. That
order matters. The function asked for group 3 knowing only the values of
groups 1 and 2, so checking a trace group by group, stopping at the first
mismatch, asks for exactly the keys a fresh computation would, and never
computes a dependency the function wouldn't have needed. Keys within a
group are independent, so they're computed in parallel and later checked
in parallel.

A trace is valid wherever every dependency has the recorded value.
Because values are deduplicated in the [store](skystore.md), "has the
recorded value" is an integer comparison of `ValueRef`s.

Traces are themselves stored in the store, encoded, compressed and
deduplicated, and hold references to the values they mention.

## The engine and its memo

The `Engine` holds:

- the **store** of values and traces;
- the **key table**;
- the **memo**: for each key, up to four recent traces (newest first),
  sharded 64 ways by key;
- a **decoded-value cache** (16 MiB by default, `BB_CACHE_MB` to change),
  so values in use aren't decompressed and decoded on every read. It's a
  two-generation approximation of LRU: values go into the young
  generation, and when it fills half the budget it becomes the old
  generation and the previous old one is dropped. Reading from the old
  generation promotes back to the young one.
- per-kind counters: computed, verified, reused, and time spent computing.

Every non-external, non-transient computation adds its trace to the memo.
Keeping several traces per key lets workspaces at different revisions of
a repository (a worktree on `main` and one on a feature branch) each find
a match.

## Graphs

A `Graph` is one workspace's view. It holds a table from `KeyId` to a
16-byte `Node`: the node's trace, its value, the revision it was last
verified at, and flags. It also holds the workspace's **globals**,
services that key functions reach through `ctx.global::<T>()`, such as
the workspace root, the change tracker and the progress reporter, and
server-wide services such as the execution backend.

### Evaluating a key

`graph.session().await.evaluate(&key)` or, inside a key function,
`ctx.compute(&key)` requests a key at the current revision. The graph
then:

1. **Returns at once** if the key's node was verified at this revision and
   isn't dirty.
2. **Joins an in-flight computation** if one is running for this key. At
   most one computation per key per graph runs at a time. Later requests
   wait on it.
3. Otherwise **starts a task** to resolve the key, which tries three
   things in order:

   - **Verify** (`Verified`): if the node has a result and isn't dirty or
     transient, check its trace's dependencies group by group at this
     revision. Each check is itself a request, so this recurses down the
     graph. If every dependency still has its recorded value, the node is
     kept. This is change pruning: a key whose inputs were recomputed to
     the same values isn't recomputed.
   - **Reuse** (`Reused`): for a non-external key, try each trace in the
     engine's memo for this key (other than the node's current one) and
     verify it the same way. The first that matches becomes the node's
     result. This is how a workspace gets a value another workspace, or a
     previous server, computed.
   - **Compute** (`Computed`): run the key's function with a fresh `Ctx`,
     which records each group of dependencies as the function reads them.
     The value (or error) is stored, a trace is built from what was
     recorded, and the trace is added to the memo.

```
request(key)
  │
  ├─ node verified at this revision? ──► return its value
  ├─ already in flight?              ──► wait for it
  └─ start task:
       ├─ node's own trace still valid?        ──► keep    (Verified)
       ├─ some remembered trace valid here?    ──► adopt   (Reused)
       └─ run compute(), record trace, memo it ──► new     (Computed)
```

A recomputed key that produces the same value gets the same `ValueRef`,
so the keys that depend on it still verify. Change pruning works across
recomputation too.

### Async functions instead of restarts

In Bazel, a `SkyFunction` that needs a value that isn't ready returns
`null`, and Skyframe runs it again from the top once the value exists.
bb's key functions are futures: `ctx.compute(&dep).await` suspends the
function and resumes it when the dependency is ready, with all its local
state intact. Nothing is computed twice, and functions read as straight-line
code.

CPU-heavy work, mostly Starlark evaluation, doesn't run on the async
runtime. Functions gather everything they need asynchronously, then run
the evaluation on a blocking thread through `bazel_loading::blocking`,
which limits concurrent evaluations to the machine's parallelism. Starlark
code that has to reach the graph mid-evaluation, such as `glob()` or a
repository rule, blocks its thread on the async runtime (`GraphAccess`,
`GraphGlobber`).

### Errors

Errors are results like values. A `SkyError` has a kind:

- `Failed`: computing the value would fail again with the same inputs.
  It's stored and kept like any value, and returned until the key's
  dependencies change.
- `Transient`: the failure might not recur, such as an I/O error, a missing
  CAS blob or a failed action (Bazel re-runs failed actions every build).
  A transient result isn't added to the memo, and anything that read it is
  marked transient too. `Graph::invalidate_transient`, called at the start
  of every command, makes them all recompute.
- `Cycle`: the key depends on itself.

A key function that panics doesn't take the server down. The panic is
caught and becomes the key's error.

### Cycles

The graph tracks which in-flight key is waiting on which. It searches for
a cycle only when every in-flight computation is waiting on another, so
nothing can make progress, which keeps the search off the common path. A
cycle found fails the waits on it with a `Cycle` error naming each key on
the path.

### Cancellation

When a command is cancelled, its futures are dropped. Each request holds
a guard while it waits on an in-flight key, and the flight counts its
waiters. When the last waiter goes away before there's a result, the
computation's task is aborted. Work that some other command still waits
for keeps running.

## Revisions and invalidation

A graph has a revision number. A `Session` evaluates at one revision, and
values are only treated as current at the revision they were verified at.

When something outside the graph changes, `Graph::invalidate(&keys)` marks
those (external) keys' nodes dirty and starts a new revision. Invalidation
waits for open sessions to finish, and new sessions wait for invalidation,
so a command never sees a half-invalidated graph.

Nothing is eagerly propagated upwards. After the revision bump, every
node is stale, and the next request for any key verifies it from its
dependencies up. Unchanged parts of the graph verify quickly, and only
what really changed is recomputed. At the Bazel layer, each command
begins by re-statting the files the workspace has read and invalidating
the file keys of those that changed (`bazel/fs/changes.rs`).

## Parts

Some values must be live, because an analyzed target holds Starlark
objects, but much of what they hold can be encoded. Such a value keeps its
encodable parts as `Stored<T>` handles (`part.rs`), created with
`ctx.store(&part)`. The part's bytes live in the engine's store,
compressed with a dictionary trained on parts of the same type and
deduplicated across every value that holds them. A part is decoded when
read. The bulk of each action (its mnemonic, progress message, outputs,
and command line and environment) is kept this way, while its inputs stay
as shared `InputSet`s. A configured target with hundreds of actions holds
hundreds of small handles rather than hundreds of decoded actions, and
an action is decoded only when it runs.

## Concurrency

There's no global lock. A graph's node table is behind one mutex held only
for table operations, never across an await. The engine's memo and cache
are sharded, and the store has 16 shards. Many sessions can evaluate a
graph at once, and many graphs can share an engine.

## Statistics

`Engine::stats()` reports the key table's size, the memo's keys and
traces, the store's statistics, the cache's size, and per kind: how many
values were computed, verified and reused, time spent computing, and the
bytes of its values and traces. `Graph::stats()` reports a workspace's
node count and revision.
