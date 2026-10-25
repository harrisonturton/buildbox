# What bb is missing compared with Bazel

This lists the large structural pieces of Bazel that bb doesn't have yet. It
leaves out small interface differences, such as individual flags or
`ctx` fields. It was written on 2026-10-25 against Bazel 9.3.1, and is
roughly in order of how many real repositories each gap blocks.

## 1. Configurations and transitions

- **No general options model.** `BuildOptions` (`buildbox/bazel/config/config.rs`)
  is a fixed struct: compilation mode, cpu, platforms, `--define`s, a few
  other values, Starlark flags and action env. Bazel's
  `//command_line_option:*` namespace has nothing behind it, so a
  transition has nothing to read or write. This has to come first.
- **Transitions are no-ops.** `dep_config` maps `Cfg::Transition` to the
  parent's configuration (`buildbox/bazel/analysis/configured.rs`).
  Incoming rule transitions (`rule(cfg = ...)`) are ignored,
  `ctx.split_attr` is always empty, and `transition()` and
  `analysis_test_transition()` return opaque values
  (`buildbox/bazel/starlark/bzl_api.rs`).
- **The exec configuration is the host's.** `BuildConfiguration::exec()`
  sets the platform to `host_platform` rather than to the execution
  platform toolchain resolution chose.

rules_go, rules_rust, rules_oci, rules_python, platform-specific binary
wrappers and multi-arch images all rely on transitions.

## 2. Aspects

`aspect()` returns an opaque value and `ctx.aspect_ids` is always empty.
There is no propagation along attributes, no `--aspects`, no
`required_providers`, and no aspects on aspects.

This blocks protobuf's language rules (`cc_proto_library` and its siblings
are built on aspects), rules_rust's clippy, rustfmt and rust-analyzer
support, rules_lint, IDE plugins and `compile_commands.json` generators.

## 3. Execution platforms and exec groups

- **Exec groups:** `ctx.exec_groups` knows which groups a rule declares,
  but fails when asked for one's context, since no toolchains are resolved
  per group. Actions drop their `exec_group` argument
  (`buildbox/bazel/analysis/actions.rs`).
- **Remote platform:** the REAPI `Platform` of every action is empty
  (`buildbox/bazel/execution/spawn.rs`), so `exec_properties` never reach a
  remote executor.
- **Execution requirements:** they're recorded on actions, but nothing acts
  on `no-sandbox`, `local`, `requires-network`, `no-remote` and the like.
  There are no spawn strategies.

Together with section 1, this means remote execution on a platform other
than the host doesn't work.

## 4. Builtin APIs for rule sets other than C++

Only C++ has a real implementation. `java_common`,
`proto_common_do_not_use`, `apple_common`, `testing` and `config` are opaque
values (`buildbox/bazel/starlark/opaque.rs`), and there's no `py_internal` or
`JavaInfo`. This leaves out Java and Kotlin, Python, proto, Apple, Android,
and analysis tests (`testing.analysis_test`, which rules_testing and
skylib's `unittest` use). `coverage_common` is real, but only as Bazel
behaves with coverage off (section 6).

A rule implementation fails if it calls, iterates or searches an opaque
value, or returns one as a provider, so a missing API is an error rather
than a silently wrong analysis. Two gaps remain:

- Opaque values are still truthy, since starlark-rust's `to_bool` can't
  fail: `if java_common.something:` takes the branch.
- Apple's late-bound `_xcode_config` is a lenient opaque value, which rules
  can use freely: rules_cc reads it unconditionally on every platform.
  Bazel 9 analyzes `@local_config_xcode//:host_xcodes` (apple_support's
  `xcode_config`, through `@bazel_tools`' `xcode_configure` extension)
  instead, which needs `apple_common.XcodeVersionConfig` and the Apple
  platform types.

## 5. Query tooling

- No `cquery` or `aquery`. cquery depends on section 1.
- `query` has no machine-readable output formats (`proto`,
  `streamed_proto`, `jsonproto`, `xml`), which IDE plugins, Gazelle and
  most query-based tools consume.
- No `--universe_scope`, so no `allrdeps`.

## 6. The test runner

`--runs_per_test`, `--flaky_test_attempts` and the `shard_count` attribute
are accepted but have no effect: nothing sets `TEST_TOTAL_SHARDS`. There's
no `coverage` command, no `coverage_common`, and no LCOV collection or
merging.

## 7. Persistent workers

There are no persistent or multiplex workers (`supports-workers`). Java,
Kotlin, Scala and TypeScript builds still build, but their compile actions
are much slower.

## 8. Build reporting

There's no Build Event Protocol (`--bes_backend`,
`--build_event_json_file`), no execution log and no `--profile` JSON trace.
CI integrations and remote build services depend on these.

## 9. Bzlmod and fetching

- **Lockfile:** `MODULE.bazel.lock` is neither read nor written;
  `--lockfile_mode` is accepted and ignored.
- **Overrides:** `git_override` and `multiple_version_override` fail
  (`buildbox/bazel/bzlmod/resolve.rs`). A `single_version_override` with
  patches replaces the registry's `patch_strip` with its own, so the
  registry's patches apply with the wrong strip when the two differ.
- **Commands:** there's no `mod`, `fetch`, `vendor` or `sync`.
- **Authentication:** downloads support neither `.netrc` nor credential
  helpers, so private registries and authenticated `http_archive`s fail.
- **Stamping:** `--workspace_status_command` isn't run, so the workspace
  status files hold fixed values.

## 10. Visibility

Analysis doesn't check target visibility, so a build can succeed in bb that
Bazel rejects.

## Suggested order

1. A general options model, then transitions, then the exec configuration
   from the resolved execution platform. cquery largely follows.
2. Aspects, which much of the remaining ecosystem depends on.

## Not yet audited

Runfiles edge cases, tree artifacts and action conflict detection may hide
further gaps.
