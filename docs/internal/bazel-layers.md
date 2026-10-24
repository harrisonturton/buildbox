# The Bazel layers

Everything Bazel-specific lives under `buildbox/bazel/`, one Bazel package
per layer, on top of the [engine](skyframe.md). Most layers contribute
key types to the graph. This page goes bottom up, says what each layer is
for, and lists its keys.

Keys marked **external** read the world outside the graph (files, the
environment, test runs). They're never shared between workspaces and are
recomputed when invalidated. Every other key is shared through traces.

## Foundations: names, options, configurations

**`bazel/label`.** Repository names, package ids, labels and target
patterns. Each is a few [interned](intern.md) ids, so a `Label` is 12
bytes (repository, package path, name) and compares as integers. This
layer also holds repository mappings, the table of how one repository
names the others (`RepoMappingTable`).

**`bazel/options`.** Bazel's command line: startup options, the command,
options from `.bazelrc` files (with `import`, `try-import` and `--config`)
and the command line, and the residue (target patterns or a query
expression). Only the options bb implements are declared. Unknown options
are skipped, not rejected.

**`bazel/config`.** Build configurations. `BuildOptions` holds what
decides how targets are built: compilation mode, CPU, target and host
platforms, whether it's an exec configuration, `--define`s, Starlark
flags, `--action_env`, and the other options `config_setting` can match.
Each configuration is interned, so a configured target refers to its
configuration by a 4-byte `ConfigId`. `ConfigId::get` gives the decoded
configuration with its derived names, such as the `bazel-out` directory
(`k8-fastbuild`, `k8-opt-exec-ST-…`), which match Bazel's. Configurations
are never discarded, so targets in different configurations coexist in the
graph.

## Files

**`bazel/fs`.** The filesystem as the graph sees it.

| Key | Value | |
|---|---|---|
| `FILE_STATE` | Missing, file (size, executable), directory or other | external |
| `DIRECTORY_LISTING` | A directory's entries and their kinds | external |
| `FILE_CONTENT` | A file's bytes | external |
| `GLOB` | The files of a package matching `glob()` patterns | |

Paths are `RootedPath`s: a `Root` and an interned relative path. The root
`Root::Workspace` means "this graph's workspace, wherever it is", resolved
through the graph's `Workspace` global, so two worktrees at different
paths have identical keys and can share everything computed from them.
Fetched repositories are `Root::Dir(path)`.

Change detection (`changes.rs`): before a file key reads a path, it
records the path's `stat` signature (existence, type, size, mtime, ctime,
inode, and the symlink's own ctime). At the start of each command,
`FileChanges::changed` re-stats every recorded path, 512 paths per blocking
task, in parallel, and the workspace invalidates the file keys of those
that differ. Recording *before* reading means an edit made while the file
was being read is still seen as a change next time.

## Starlark

**`bazel/starlark`.** Bazel's dialect of Starlark, on
[starlark-rust](https://github.com/facebook/starlark-rust) (patched
through `third_party/starlark`). It provides the builtins BUILD and `.bzl`
files use: `rule()`, `attr`, `provider()`, `depset`, `select()`,
`Label`, `json`, `subrule()`, `repository_rule()`, `module_extension()`,
macros, and the native rules. Evaluating a BUILD file accumulates targets
in a `PackageBuilder`. Struct-like values (provider instances) intern
their list of field names, so values made in the same place share it.
`hash()` matches Java's `String.hashCode`, because rules name outputs with
it.

This layer doesn't depend on the graph. Callers hand it what it needs:
loaded modules, repository mappings, a globber.

**`bazel/packages`.** Packages and targets as plain data: rules with
their attribute values, source files, output files and package groups.
They're independent of Starlark, so queries over packages don't need an
interpreter, and they're encoded compactly so that loaded packages are
stored compressed.

## Loading

**`bazel/loading`.** Turning BUILD and `.bzl` files into packages.

| Key | Value |
|---|---|
| `REPOSITORY_DIRECTORY` | Where a repository's files are (fetching it if needed) |
| `REPOSITORY_MAPPING` | How a repository sees other repositories |
| `PACKAGE_LOOKUP` | Whether a directory is a package, and its BUILD file |
| `BZL_LOAD` | An evaluated `.bzl` module (live) |
| `PACKAGE` | A loaded package (encoded) |
| `PACKAGES_UNDER_DIRECTORY` | The packages beneath a directory, for `//foo/...` |
| `IGNORED_DIRECTORIES` | The directories a repository's `.bazelignore` lists |
| `RULE_CLASS` | A rule class's schema, for queries |
| `BUILTINS` | Bazel's `@_builtins`, evaluated once per server |
| `REGISTRATIONS` | Registered toolchains and execution platforms |

A `BZL_LOAD` value is live, a starlark-rust `FrozenModule`, so it's
stored by fingerprint: a hash of the file's label and source, its
repository mapping, and the fingerprints of the modules it loads. Two
worktrees with the same file and the same loads share one module.

`@_builtins` and `@bazel_tools` come from an installed Bazel (the version
in `.bazelversion`, currently 9.3.1): `@bazel_tools` from its
`embedded_tools`, and `@_builtins` extracted from its server jar into the
server's directory on first use. bb runs Bazel's own Starlark
implementations of builtin rules, such as rules_cc's C++ rules, on top of
native APIs it implements itself.

Loading depends on a `Repositories` service for where repositories are
and how they map, which the Bzlmod layer provides. That keeps loading
independent of how repositories are defined.

## External repositories

**`bazel/fetch`.** Downloads, verified against their expected checksum
and kept in a cache addressed by SHA-256 (shared by every workspace, and
optionally by other servers), archive extraction, and applying patches as
`patch -p<N>` and `git apply` do.

**`bazel/bzlmod`.** Module resolution and repositories.

| Key | Value | |
|---|---|---|
| `ROOT_MODULE` | The evaluated root `MODULE.bazel` | |
| `REGISTRY_FILE`, `REGISTRY_MODULE` | Files and modules from registries | |
| `BAZEL_DEP_GRAPH` | The resolved dependency graph (minimal version selection) and canonical repository names | |
| `MODULE_EXTENSION` | An evaluated module extension and the repositories it defines | |
| `REPOSITORY_SPEC` | How a repository is made | |
| `REPOSITORY_SPEC_FETCH`, `REPOSITORY_FETCH` | A fetched repository's directory | |
| `REPO_ENV` | The environment repository rules see: the client's, with `--repo_env` | external |

Repositories are fetched into a **machine-wide directory, named by their
spec**, so a repository is fetched once per machine however many
workspaces use it, and survives server restarts. Repository rules and
module extensions run Starlark on blocking threads and reach the graph
through `GraphAccess`, so what they read is recorded as dependencies.

## Analysis

**`bazel/analysis`.** Running rules in configurations to find the actions
that build their files.

| Key | Value |
|---|---|
| `CONFIGURED_TARGET` | A target analyzed in a configuration: providers and actions (live) |
| `CONFIG_CONDITION` | Whether a `config_setting` matches a configuration |
| `PLATFORM`, `CONSTRAINT_VALUE` | Platforms and their constraints |
| `REGISTERED_TOOLCHAINS` | The toolchains a workspace registers |
| `TOOLCHAIN_RESOLUTION` | The toolchain chosen for a toolchain type on a platform |

Analyzing a rule (`configured.rs`):

1. Load the package and find the rule. Resolve its attributes in the
   configuration: fill in defaults and resolve `select()`s.
2. Check `target_compatible_with`. An incompatible target, or one with an
   incompatible dependency, has no providers or actions, and records the
   chain of dependencies that made it so.
3. Analyze dependencies in their configurations (exec transitions for
   tools), resolve toolchains, and fetch the repository mappings the
   implementation needs. All of this is asynchronous graph work.
4. Run the rule's implementation with a `ctx` on a blocking thread. The
   implementation is a `.bzl` function, a builtin from `@_builtins`, or a
   native rule.
5. Build the `ConfiguredTarget`: its providers as a frozen Starlark value,
   its actions as `StoredAction`s, and a sorted table from output path to
   generating action.

A `ConfiguredTarget` is live, so it's stored by a fingerprint of what it
was analyzed from: the key, the rule, the implementation, and its
dependencies' and toolchains' fingerprints.

Memory-saving structures:

- **`InputSet`** (`inputs.rs`) is Bazel's nested set for action inputs.
  It keeps the shape of the depsets it came from, and equal nodes are one
  shared `Arc` across every action and target in the process (through a
  `Sharing` table of weak references, `shared.rs`). Sets are flattened
  only when an action runs.
- **`StoredAction`** keeps an action's inputs as an `InputSet` and its
  runfiles trees as shared `Arc`s, and the rest (mnemonic, progress
  message, outputs, command line, environment) as a compressed
  [part](skyframe.md#parts) in the engine's store.

`analysis/cc/` implements the native C++ API that rules_cc's Starlark
rules are written against under Bazel 9 (`cc_internal`, `cc_common`,
`CcInfo`, toolchain features and build variables), computing command lines
from toolchain features as Bazel's `CcToolchainFeatures` does.

## Execution

**`bazel/execution`.** Running actions.

| Key | Value | |
|---|---|---|
| `ACTION_EXECUTION` | The digests of one action's outputs | |
| `SOURCE_DIGEST` | A source file's digest | external |
| `SOURCE_TREE` | A source directory's files and digests | |
| `TEST` | A test's result | external |

Actions run through a **`Backend`**: a CAS, an action cache and an
executor, each a trait object. The server wires in its own in-process
implementations (actions run sandboxed or in containers on this machine)
or clients of a remote execution service, optionally a remote cache in
front of local execution (`buildbox/server/server.rs`). Execution code
doesn't know which.

Running a spawn action (`spawn.rs`):

1. Wait until every input is generated, holding none of their digests.
2. Take a staging permit, which bounds how many actions have input trees
   in memory.
3. Collect inputs: source digests and generating actions' outputs, plus
   tools' runfiles trees.
4. Check the `LocalActionCache` by a cheap fingerprint of the command and
   inputs. On a hit whose outputs are still in the CAS, done.
5. Build the REAPI input tree, upload missing sources, and execute through
   the backend, which checks the REAPI action cache first.
6. A failing action reports its output and fails with a *transient*
   error, so the next build runs it again, as Bazel does.

Write, template-expansion and symlink actions run in-process without a
spawn.

The value of an action key is only digests. Outputs are written to the
output tree when a build asks for them (`materialize.rs`).

## Query

**`bazel/query`.** Bazel's query language: a parser, an evaluator and
Bazel's output formats. The `Universe` loads the targets a query reads
through `PACKAGE` keys, and can run over a `Session` or a `Ctx` (through
the `Compute` trait), so queries share loaded packages with builds and
with other workspaces.

## Progress and terminal output

**`bazel/progress`.** A per-workspace graph service that key functions
report to: packages loaded, targets configured, and actions started,
running and finished (and by which runner, such as `linux-sandbox`,
`remote` or `disk cache hit`), plus what actions printed and why they
failed. Commands attach a tracker while they run.

**`bazel/ui`.** Pure rendering of Bazel's terminal output: `INFO:` and
`ERROR:` messages, the progress bar Bazel redraws beneath them, and
summaries such as the critical path and process counts, worded exactly as
Bazel words them. Output never says "Bazel": it uses Bazel's formats but
names itself `bb`. It renders text and does no I/O.

## The server

**`bazel/server`.** The commands and the RPC service.

- `BazelServer` (`server.rs`) owns the engine, the workspaces, the saved
  state and the running commands. It opens the engine before anything
  else, wires in server-wide services (repositories, Bzlmod config,
  builtins, source digests, local action cache, backend), and after each
  command saves state, checkpoints the engine when idle, and returns
  freed memory.
- `Workspaces` (`workspace.rs`) maps workspace roots to `Workspace`s,
  each with its graph and per-workspace services (root, file changes,
  client environment, written outputs, progress).
  `Workspace::refresh` runs at the start of each command: it invalidates
  transient results, the environment if it changed, and changed files.
- `command.rs` splits startup options, finds the workspace, reads
  `.bazelrc` files and dispatches: `build`, `test`, `run`, `query`,
  `info`, `clean`, `shutdown`, `dump`, `version` and `help`.
  `bb shutdown` frees only the calling workspace's graph, since other
  workspaces share the server. Help pages are Bazel's own text, limited to
  what bb supports.
- `build.rs` runs a build. Its flow is in the [overview](overview.md), and
  its overlap of analysis and execution is in [Skymeld](skymeld.md).
- `output.rs` streams a command's output back to the client and drives the
  progress bar on a 200 ms ticker.
- `written.rs` remembers what each build wrote to the output tree, so a
  build writes only outputs that changed.
- `service.rs` is the Connect RPC service: `Run` streams a command's
  output, and there are calls to inspect the server, list and cancel
  commands, and shut it down. It refuses commands from a client of a
  different build, telling it to restart the server.

## Adding a key

To add a new kind of graph node:

1. Define the key type and derive `Encode`, `Decode`, `Clone`, `Eq`,
   `Hash` and `Debug`.
2. Define its value. Derive `Encode`/`Decode` and use `encoded_value!`
   if you can. Implement `Value` with `Storage::Live` and a fingerprint
   only if the value must hold live objects.
3. Implement `Key`. Read every input through `ctx.compute`, and ask for
   independent keys together with `ctx.compute_many`. If the function
   reads anything outside the graph, set `EXTERNAL = true` and make sure
   something invalidates it.
4. Run CPU-heavy or blocking work through `bazel_loading::blocking` or
   `spawn_blocking`, never on the async runtime.
5. Return `SkyError::transient` for failures that may not recur, and
   `SkyError::new` for failures the same inputs would always produce.

The key's kind name (`KIND`) appears in statistics and `bb dump`, so name
it as Bazel names the equivalent SkyFunction where there is one.
