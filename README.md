# `buildbox`

Buildbox is a Bazel [remote execution](https://bazel.build/remote/rbe) server
for running builds on a machine you control. It implements the
[Remote Execution API](https://github.com/bazelbuild/remote-apis) (REAPI)
with a local action cache and local, unisolated execution, and it's designed to
be trivial to run:

```
bb up
```

I built it for reproducible builds at home, and to build with Linux-only
toolchains (e.g. Vivado) from a MacBook. It may be useful to you if:

* You want remote execution and caching without the yak shaving.
* You trust your build inputs: actions run on the host with no isolation.
* You're happy with *mostly* hermetic and reproducible.
* One machine is enough.

This is a personal utility, shared in case others find it useful. I'll gladly
accept contributions, but make no stability, support or response time
guarantees.

> **Security:** buildbox has no authentication or TLS, and runs whatever
> actions it is sent with your user's permissions. By default it only listens
> on the loopback address. Don't expose it to networks you don't trust; to
> reach it from other machines, use something like
> [Tailscale](https://tailscale.com).

## Installing

Buildbox is built with Bazel. Install
[bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; every toolchain,
including the C toolchain and sysroot, is downloaded by Bazel.

To build and install buildbox as `bb` in `~/.local/bin`:

```
./install.sh
```

Set `PREFIX` to install elsewhere: `PREFIX=/usr/local ./install.sh` installs to
`/usr/local/bin`.

## Using it

Start the server:

```
bb up
```

Then point Bazel at it, in your project's `.bazelrc`:

```
build --remote_executor=grpc://[::1]:50051
```

Actions run on the server's host, so toolchains need to work there. Toolchains
that Bazel downloads (rather than ones installed on the host) make this easy.

While the server runs, the CLI can inspect it:

```
$ bb status
buildbox 0.1.0, up 2h 5m
CAS           34102 blobs, 9.2 GiB      /home/me/.cache/buildbox/cas
Action cache  381 entries, 161.6 KiB    /home/me/.cache/buildbox/ac
Actions       48 running, 4 queued, limit 48

$ bb actions
ID      STATE    TIME      COMMAND
52      running  822ms     /bin/bash -c source external/bazel_tools/tools/genrule/genrule-setup.sh; ...
```

## Configuration

Settings come from built-in defaults, then an optional config file, then
command-line flags, each overriding the last. `bb config` prints the
result.

The config file is `$XDG_CONFIG_HOME/buildbox/config.toml` (usually
`~/.config/buildbox/config.toml`) if it exists, or the file given with
`--config`. Every setting is optional:

```toml
# The address to listen on.
listen = "[::1]:50051"

# Holds the CAS, action cache and sandboxes, unless they're set individually.
# Defaults to $XDG_CACHE_HOME/buildbox, usually ~/.cache/buildbox.
root = "~/.cache/buildbox"

# Where blobs, action results and running actions live.
cas_dir = "~/.cache/buildbox/cas"
action_cache_dir = "~/.cache/buildbox/ac"
sandbox_dir = "~/.cache/buildbox/sandbox"

# How many actions may run at once. Defaults to the number of CPUs.
max_concurrent_actions = 48

# Keep each action's directory after it finishes, for debugging.
keep_sandboxes = false

# Append a trace of everything the server does to this file. Off by default.
trace_file = "~/.cache/buildbox/traces.jsonl"
```

`~` is your home directory, and relative paths are relative to the config file.
Each setting has a matching flag, e.g. `bb up --root /data/buildbox -j 16`;
see `bb up --help`.

Keep `cas_dir` and `sandbox_dir` on the same filesystem, so inputs can be hard
linked into sandboxes rather than copied.

## Tracing

With `trace_file` set, the server records how long each part of its work
takes, as OpenTelemetry spans, and appends them to the file as OTLP JSON, one
batch per line. Every request Bazel makes during one invocation (one
`bazel build`, say) is part of one trace, under a `bazel invocation` span,
since Bazel tags requests with its invocation ID. Each request's span records
the action it's for (`bazel.action_id`), and an `Execute` breaks down into
checking the cache, waiting for a slot, staging inputs, running the command
and collecting outputs. A client that sends a W3C `traceparent` header gets
its requests in its own trace instead.

An invocation's span is written once it has made no requests for a minute, or
when the server stops; the rest are written within a few seconds of ending.
The file isn't rotated.

To find the slowest actions:

```
jq -c '.resourceSpans[].scopeSpans[].spans[]
  | select(.name | endswith("Execution/Execute"))
  | {action: (.attributes[] | select(.key == "action.digest") | .value.stringValue),
     ms: (((.endTimeUnixNano | tonumber) - (.startTimeUnixNano | tonumber)) / 1e6)}' \
  ~/.cache/buildbox/traces.jsonl | jq -s 'sort_by(-.ms) | .[:10]'
```

To browse traces in a UI, run an OpenTelemetry Collector with the
`otlpjsonfile` receiver pointed at the file, exporting to Jaeger or similar.
`TRACE_LEVEL=debug` adds a span for every blob the CAS reads and writes, and
`LOG_LEVEL` sets the level of the log on stderr.

## How it works

Buildbox implements the parts of REAPI v2 that Bazel uses: the
ContentAddressableStorage, ByteStream, ActionCache, Execution and Capabilities
services.

**Storage.** Each blob is a read-only file named after its SHA-256 hash, under
`<cas_dir>/blobs`. Uploads are written to a temporary file, checked against
their expected digest, and renamed into place, so a partial or corrupt upload
is never visible. Each action result is a file named after the action's hash,
under `<action_cache_dir>/entries`. Cached results are only returned while
every output they refer to is still in the CAS.

**Execution.** To run an action, buildbox makes a fresh directory under
`<sandbox_dir>`, hard links the action's inputs into it from the CAS, and runs
the command there with exactly the environment the action specifies. The
command runs in its own process group, so on exit, timeout or cancellation
everything it started is killed. Outputs are stored in the CAS, and results
of successful actions are cached. Up to `max_concurrent_actions` run at once;
the rest queue.

Actions run on the host with your user's permissions and can see everything on
it. Inputs are hard linked and read-only, so an action that changes an input's
permissions or contents in place changes the stored blob too.

**Code layout.** All Rust code is under `buildbox/`, one Bazel package per
component, with interfaces and implementations as separate targets:

| Package | Contents |
|---|---|
| `buildbox/cli` | The `buildbox` command. |
| `buildbox/server` | Wires the stores, executor and services together. |
| `buildbox/config` | Layered configuration. |
| `buildbox/reapi` | The REAPI gRPC services. |
| `buildbox/admin` | The admin API (`buildbox.admin.v1`) the CLI talks to. |
| `buildbox/cas` | The `Cas` trait, plus filesystem and in-memory stores. |
| `buildbox/actioncache` | The `ActionCache` trait, plus filesystem and in-memory stores. |
| `buildbox/executor` | The `Executor` trait, and the local executor. |
| `buildbox/scheduler` | Limits and tracks running actions. |
| `buildbox/fsutil` | Filesystem helpers. |
| `buildbox/telemetry` | Logging, and recording spans to the trace file. |

`bazel/` holds Bazel definitions (toolchains, the version number), and
`third_party/` holds the external dependencies, one `*.MODULE.bazel` file
each, included from `MODULE.bazel`.

## Limitations

* Nothing is ever deleted from the CAS or action cache.
* Only REAPI v2.0 is supported: no `output_paths`, compression, or Remote Asset
  API.
* Actions aren't isolated from the host or each other.
* One machine only.

## Development

```
bazel build //...
bazel test //...
```

Warnings are errors. See [AGENTS.md](AGENTS.md) for the engineering
guidelines.

### Rust Analyzer

Because this repository is built with Bazel, rust-analyzer needs a generated
`rust-project.json` to index it. Generate it with:

```
bazel run @rules_rust//tools/rust_analyzer:gen_rust_project -- //buildbox/...
```

Re-run it after adding or removing targets or dependencies.
