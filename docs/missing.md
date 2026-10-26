# What bb is missing compared with Bazel

This lists the large structural pieces of Bazel that bb doesn't have yet. It
leaves out small interface differences, such as individual flags or
`ctx` fields. It was written on 2026-10-25 against Bazel 9.3.1, and is
roughly in order of how many real repositories each gap blocks.

## 1. Configurations and transitions

- **Native options.** Configurations are made of native options and
  Starlark flags (`buildbox/bazel/config/options.rs`): a registry of the
  native options bb knows, with Bazel 9.3.1's types, defaults and parsing.
  It has those `config_setting`s match in the rule sets bb has met, and
  every one Bazel's exec transition reads or writes; a `config_setting` or
  transition on any other fails rather than misbehaving. Most of them only
  carry their values, which transitions and `config_setting`s read.
- **Transitions are partly there.** Starlark transitions work on
  attributes and on rules (`rule(cfg = ...)`), including splits and
  `ctx.split_attr` (`buildbox/bazel/analysis/transition.rs`), and their
  output directories are named as Bazel names them, against the top-level
  configuration. Still missing:
  - `analysis_test_transition`, and Bazel's own transitions that bb only
    stands in for, such as Apple's splits.
- **The exec configuration** comes from Bazel's own Starlark exec
  transition, run for the execution platform toolchain resolution chooses
  for each rule, from the platforms `--extra_execution_platforms` and
  modules register, with the toolchains `--extra_toolchains` and modules
  register. Toolchains' own tools are for the execution platform of the
  rule that uses them, as Bazel's toolchain transition gives, and output
  directories are named as Bazel names them.

rules_go, rules_rust, rules_oci, rules_python, platform-specific binary
wrappers and multi-arch images all rely on transitions, and most of them on
the exec configuration being right.

## 2. Aspects

Starlark aspects work as Bazel's do:

- attached with `attr.label(aspects = [...])`, taking parameters from the
  rule's attributes of the same names and types;
- propagated along `attr_aspects`, including `*`, and to the toolchains of
  the types `toolchains_aspects` names, or every type for `*`, each list
  given or computed for each target by a function;
- applied only to rules whose `provides` satisfy `required_providers`, and
  that their `propagation_predicate` accepts; and with
  `apply_to_generating_rules`, to the rules generating the files they
  propagate to;
- applied after the aspects they `requires`, and seeing the providers of
  those and of the aspects whose `provides` satisfy their
  `required_aspect_providers`, keyed as Bazel keys them by the aspects
  they see;
- with private and exec attributes, toolchains and exec groups;
- with `ctx.rule`, its `toolchains` giving the providers aspects gave
  them, and `ctx.aspect_ids`; and their providers merged into the
  dependent's view of the target, output groups included;
- applied at the top level with `--aspects` and `--aspects_parameters`,
  built with `--output_groups` and reported as Bazel reports them;
- checked where they're defined, attached and used, and failing as Bazel's
  do, with Bazel's tracebacks and wording, as rules' implementations now
  do too.

Bazel 9 has no native aspects that users apply, only its validation
aspect, which bb doesn't have.

`cquery` counts what an aspect's own attributes depend on as dependencies
of the rule that attaches it, and `aquery` lists aspects' actions with
the aspects that made them, as Bazel's do with `--include_aspects`.

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

`query`, `cquery` and `aquery` share one query language
(`buildbox/bazel/query`): `query` reads the package graph, and `cquery` and
`aquery` the configured graph analysis builds (`buildbox/bazel/cquery`).
Their outputs match Bazel 9.3.1's on the formats people and tools read,
`proto`, `jsonproto` and `xml` included. Still missing:

- **Configuration checksums** are bb's own hash of its options, not
  Bazel's, so `cquery` names configurations by different ids, and bb has
  no test-trimmed configurations: where Bazel analyzes non-test targets in
  a configuration without test options, bb uses the same one.
- **aquery's action keys** are bb's hash of the action, not Bazel's.
- rules_rust names outputs by `hash(repr(label))`, and bb's `repr` of a
  `Label` is its canonical form where Bazel's is `Label("//pkg:name")`, so
  those names differ. Changing it needs starlark-rust to format labels
  with `str()` in `%s` and `print()`, as Bazel's Starlark does.
- `cquery --output=transitions`, `--show_config_fragments`, and the
  Starlark output's `FileProvider` and `FilesToRunProvider`.
- Proto outputs leave out the internal attributes bb doesn't model, such
  as `:action_listener`, `transitive_configs` and
  `$rule_implementation_hash`.
- Error messages for invalid regular expressions are Rust's, not Java's.
- Where Bazel's own order varies from run to run, as for graphs, ranks and
  `cquery`'s results, bb prints results in one of the orders Bazel may:
  `--order_output=full`'s, or by label and configuration.

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

The counts of packages loaded and targets configured that analysis reports
differ from Bazel's, which counts more targets and fewer packages: on a
small workspace, Bazel reports 5 packages and 13 targets where bb reports
69 and 4. Aspect applications are counted as Bazel counts them.

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

## 10. Checking dependencies

Analysis doesn't check that dependencies are what their attributes allow:
`allow_files`, `providers`, `allow_rules` and `executable` aren't enforced.
A rule that depends on a file its attribute doesn't allow, or on one that
doesn't exist, as `attr.label()` with `":missing"` does, analyzes in bb
where Bazel fails.

## 11. Visibility

Analysis doesn't check target visibility, so a build can succeed in bb that
Bazel rejects.

## Suggested order

1. Execution platforms and exec groups (section 3).
2. Checking dependencies and visibility (sections 10 and 11).

## Not yet audited

Runfiles edge cases, tree artifacts and action conflict detection may hide
further gaps.
