# Skymeld: overlapping analysis and execution

"Skymeld" is Bazel's name for merging the analysis and execution phases,
so that a target's actions can start running before every other target
has been analyzed. In Bazel it's controlled by
`--experimental_merged_skyframe_analysis_execution`, which defaults to
true in current Bazel.

bb does the same, but there's no separate module for it. It falls out of
two choices: **actions are keys in the same graph as analysis**, and **the
build command hands each top-level target to execution as soon as it's
analyzed**. This page explains both, and the memory bounds that keep the
overlap from blowing up.

The code is in `bazel/server/build.rs` (`run_command`, `head_starts_of`,
`head_start`, `analyze`) and `bazel/execution/` (`ActionKey`, `spawn.rs`).

## One graph for everything

Bazel historically built a separate action graph after analysis and ran it
in an execution phase. In bb, there's only the [skyframe](skyframe.md)
graph:

- `CONFIGURED_TARGET` keys analyze targets. A configured target's value
  holds its providers and its actions (as `StoredAction`s), and a table
  from each output path to the index of the action that generates it.
- `ACTION_EXECUTION` keys (`ActionKey { owner, index }`) run one action.
  An action key reads its owning configured target, gets the action, and
  for each input either hashes the source file (`SOURCE_DIGEST`) or
  requests the `ActionKey` that generates it. Its value is just the
  digests of its outputs. The bytes are in the CAS.
- `TOP_LEVEL` keys work out, once per configured target, which generated
  files building that target at the top level means: its default outputs,
  plus its runfiles if it's executable. This flattening is cached, so a
  rebuild of unchanged targets doesn't flatten runfiles again.

Because execution is graph evaluation, everything the graph does for
analysis it does for actions too: in-flight deduplication (two targets
needing the same action run it once), change pruning (an action whose
inputs' digests are unchanged isn't re-run or even looked up), sharing
across workspaces through traces, and cancellation when a command is
dropped.

## Head starts

`run_command` runs two things concurrently:

```
analyze_and_build ─┬─ load patterns ─ analyze all top-level targets ─ execute all ─ write outputs
                   │        each analyzed target ──┐
                   │                               ▼  (unbounded channel)
head_starts_of ────┴───────────────────── head_start(target): evaluate TOP_LEVEL,
                                          then every ACTION_EXECUTION it needs
```

1. `analyze` requests every top-level `ConfiguredTargetKey` at once. As
   each one finishes, if it's compatible with the target platform, it's
   sent down a channel.
2. `head_starts_of` takes each target off the channel and starts a
   `head_start` for it: evaluate its `TOP_LEVEL` key, find the actions
   that generate those files, and evaluate their `ActionKey`s.
3. Meanwhile `analyze_and_build` keeps going in Bazel's order. It waits for
   analysis to finish, reports analysis errors and the "Analyzed N
   targets" line, then asks for every top-level artifact's action itself.

The second request for an action finds it already in flight or done, so
the head start changes only *when* actions run, not what's reported or in
what order. Output matches Bazel's phase-by-phase reporting, while actions
start as early as Skymeld would start them.

If analysis fails without `--keep_going`, `analyze_and_build` returns an
error. `run_command` still joins `head_starts_of`, so the command ends
only once the actions already started are done, as Bazel's does. Under
`--nobuild` the channel isn't created and nothing runs.

## Keeping the overlap bounded

Starting everything as early as possible can mean thousands of actions
waiting at once. Two things keep that cheap:

- **Waiting actions hold almost nothing.** Before an action takes a slot,
  it waits for every input to be generated (`ready` in `spawn.rs`),
  keeping none of their digests. An action waiting on other actions holds
  only itself, not an input tree.
- **Staging is limited.** Building an action's input Merkle tree,
  uploading missing source files and running the action all happen while
  holding a permit from the backend's staging semaphore (`Backend::stage`).
  There are twice as many permits as actions the executor can run at once
  (`STAGED_PER_SLOT` in `buildbox/server/server.rs`), so the next action
  is ready when one finishes, and the memory spent on input trees is
  bounded however many actions are ready.

Analysis is bounded separately: Starlark evaluation runs on blocking
threads behind a semaphore sized to the machine's parallelism
(`bazel_loading::blocking`), since it's CPU-bound and each evaluator holds
a megabyte of stack.

## What's different from Bazel's Skymeld

The effect is the same: execution doesn't wait for the whole analysis
phase. The differences are in what surrounds it:

- **Action results are shared between workspaces.** An `ActionKey` is
  reused from another workspace's trace when its inputs have the same
  digests there, so an action that's unchanged in another worktree isn't
  run or even looked up in the action cache again.
- **There's no separate action graph to build.** Actions are found from
  configured targets on demand, through each target's output-to-action
  table.
