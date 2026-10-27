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
- **Execution requirements:** actions take them from
  `execution_requirements` and their rules' tags, as Bazel gives them.
  bb runs every action in a sandbox with the host's network, so
  `requires-network` holds and `no-cache` is honoured, but actions and
  tests tagged `block-network` fail, and those tagged `no-sandbox`,
  `local`, `no-remote` or `no-remote-exec` still run sandboxed, which
  their failures point out. Tests tagged `exclusive` run alone. There are
  no spawn strategies.

Together with section 1, this means remote execution on a platform other
than the host doesn't work.

## 4. Builtin APIs for rule sets other than C++, Java and Python

C++, Java and Python have real implementations of the native APIs their
rule sets are written against: `cc_common` and `cc_internal`,
`java_common_internal_do_not_use` (`buildbox/bazel/analysis/java`), which
compiles with the toolchain's JavaBuilder and Turbine as Bazel does, and
`py_builtins` (`buildbox/bazel/analysis/python.rs`). rules_java,
rules_python and protobuf's `proto_library`, `java_proto_library` and
prebuilt `protoc` build, and their actions match Bazel 9.3.1's. So do
analysis tests: the `testing` module, `rule(analysis_test = True)`,
`analysis_test_transition` and `Target.actions`, which rules_testing and
skylib's `unittest` use. Bazel reduces the classpath of `Javac` to what
the dependencies' `.jdeps` say is used; bb runs it with the full one.

`apple_common` and `config` are still opaque values
(`buildbox/bazel/starlark/opaque.rs`), which leaves out Apple and Android.
Kotlin and Scala aren't tested. `coverage_common` is real, but only as
Bazel behaves with coverage off (section 6).

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
`proto`, `jsonproto` and `xml` included. On bb's own repository, every
action `aquery` lists, and every field of it, matches Bazel's, the runfiles
actions of executables included. Still missing:

- **Configuration checksums** are bb's own hash of its options, not
  Bazel's, so `cquery` names configurations by different ids, and bb has
  no test-trimmed configurations: where Bazel analyzes non-test targets in
  a configuration without test options, bb uses the same one.
- **aquery's action keys** are bb's hash of the action, not Bazel's, and
  the nested sets of inputs its protos list are bb's, so their ids
  differ, though each action's inputs are Bazel's. Source manifests name
  files in bb's execution root.
- `cquery --output=transitions`, `--show_config_fragments`, and the
  Starlark output's `FileProvider` and `FilesToRunProvider`.
- Proto outputs leave out `transitive_configs` and
  `$rule_implementation_hash`, and give native rules every common
  attribute, where Bazel gives some, such as `alias` and
  `constraint_value`, fewer.
- `cquery` analyzes `_license_kind` targets in an exec configuration,
  where Bazel uses the one without options.
- Error messages for invalid regular expressions are Rust's, not Java's.
- `aquery` doesn't list tests' `TestRunner` actions, since bb runs tests
  outside the action graph.
- Where Bazel's own order varies from run to run, as for graphs, ranks and
  `cquery`'s results, bb prints results in one of the orders Bazel may:
  `--order_output=full`'s, or by label and configuration.

## 6. The test runner

The `shard_count` attribute has no effect: nothing sets
`TEST_TOTAL_SHARDS`. `--test_filter`, `--test_arg`, `--test_env`,
`--test_timeout`, `--test_tag_filters`, `--build_tag_filters`,
`--build_tests_only` and `--cache_test_results` work as in Bazel, but
`--runs_per_test`, `--flaky_test_attempts`, `--test_sharding_strategy`,
`--test_size_filters`, `--test_timeout_filters` and `--test_lang_filters`
aren't implemented (section 11). bb's action cache outlives `clean`, so
tests that passed before it are still cached after it. There's
no `coverage` command, no `coverage_common`, and no LCOV collection or
merging.

## 7. Persistent workers

There are no persistent or multiplex workers (`supports-workers`). Java,
Kotlin, Scala and TypeScript builds still build, but their compile actions
are much slower, and each prints the JVM's startup warnings, which a
worker would print once to its log.

## 8. Build reporting

There's no Build Event Protocol (`--bes_backend`,
`--build_event_json_file`), no execution log and no `--profile` JSON trace.
CI integrations and remote build services depend on these.

The counts of packages loaded and targets configured that analysis reports
differ from Bazel's, which counts more targets and fewer packages: on a
small workspace, Bazel reports 5 packages and 13 targets where bb reports
69 and 4. Aspect applications are counted as Bazel counts them.

## 9. Bzlmod and fetching

- **Lockfile:** `MODULE.bazel.lock` is neither read nor written, and
  `--lockfile_mode` isn't implemented.
- **Resolution** follows Bazel 9's discovery and selection
  (`buildbox/bazel/bzlmod/selection.rs`), with `multiple_version_override`,
  dependencies that only constrain selection, yanked versions,
  `bazel_compatibility`, and the root module's direct dependencies checked
  as `--check_direct_dependencies` says. `--override_module`,
  `--override_repository` and `--ignore_dev_dependency` aren't
  implemented.
- **Overrides:** `git_override` and
  `archive_override` take the attributes of `git_repository` and
  `http_archive` that modules use, and fail on those bb doesn't
  implement, such as `build_file` and `sparse_checkout_patterns`. A
  `git_override` of a branch or tag is fetched once and kept, where Bazel
  would fetch it again.
- **Commands:** there's no `mod`, `fetch`, `vendor` or `sync`.
- **Authentication:** downloads send the credentials of the `.netrc`
  file `$NETRC` or `$HOME` points to, and `repository_ctx.download` takes
  `auth` and `headers`, but there are no credential helpers
  (`--credential_helper`).
- **Stamping:** builds run `--workspace_status_command`, but the status
  files' host, user and time are fixed, and a change to the volatile
  status reruns the actions that read it, which Bazel doesn't.
- **Registries:** `--registry`, `--module_mirrors` and registries'
  `bazel_registry.json` work.
- **`flag_alias`** in MODULE.bazel has no effect, so the options it
  defines are unrecognized.

## 10. Checking dependencies and visibility

Analysis checks a rule's direct dependencies as Bazel does, with Bazel's
errors (`buildbox/bazel/analysis/dep_checks.rs`): that each is a file or
rule its attribute's `allow_files`, `allow_single_file`, `allow_rules`
and `providers` allow, that it's visible to the rule, from the `.bzl`
file defining the rule for implicit dependencies, and from where the
symbolic macros declaring the rule are defined, and that it isn't testonly
unless the rule is; with warnings for deprecated dependencies, and the
checks of `allow_empty = False`, executable attributes, duplicate labels
and `select()`s' `config_setting`s. Still unchecked:

- native rules' attributes, whose allowed files and providers bb doesn't
  know;
- aspects' implicit dependencies, and toolchains;
- Bazel's checks of output files of testonly rules, which
  `--experimental_check_testonly_for_output_files` turns on.

## 11. Options

bb checks every option against a table of Bazel 9.3.1's options and the
commands that take each (`buildbox/bazel/options/bazel_options.txt`,
generated from Bazel's source). An option Bazel doesn't take for the
command is an error, as in Bazel, unless a `common` line of a `.bazelrc`
gives it. An option Bazel takes but bb doesn't implement is an error
unless its last use leaves it at its default, so `--disk_cache=` on the
command line undoes a `.bazelrc`'s `--disk_cache`. bb implements few
options outside configurations and the output it prints; among those it
doesn't are:

- caches and remote execution: `--disk_cache`, `--remote_cache`,
  `--remote_executor` and the other `--remote_*` options (bb keeps its own
  action cache, shared by every workspace);
- `--announce_rc`, `--explain`, `--profile` and the Build Event Protocol;
- some test options (section 6), `--run_under` and `--script_path`;
- `--infer_universe_scope`, and query output options such as
  `--proto:default_values`.

Native options that only change how Bazel runs, such as
`--local_resources`, and the locations of the repository caches, since
bb's server keeps one for all workspaces, are accepted and ignored. Of
the native options configurations carry, those whose effect bb doesn't
implement, such as `--noenable_runfiles` and `--shell_executable`, are
errors unless left at their defaults
(`UNSUPPORTED_NATIVE_OPTIONS` in `buildbox/bazel/server/build.rs`).

## Suggested order

1. Execution platforms and exec groups (section 3).
2. The test options left (section 6), and running actions outside the
   sandbox and without the network (section 3).

## Not yet audited

Runfiles edge cases, tree artifacts and action conflict detection may hide
further gaps.
