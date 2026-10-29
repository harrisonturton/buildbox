# What bb is missing compared with Bazel

This lists what Bazel 9.3.1 has and bb doesn't, as of 2026-10-28. It's
ordered roughly by how many real repositories each gap blocks, and leaves
out differences in wording and output that only a careful diff would see.
Paths are relative to `buildbox/bazel/`.

Much is already done and isn't listed: loading, Bzlmod resolution,
Starlark rules and aspects (attached, propagated, required and top-level
with `--aspects`), Starlark transitions and splits, toolchain resolution
with optional toolchains and `target_settings`, incompatible-target
skipping, analysis tests, `select()` and `config_setting`, visibility and
dependency checks, `query`/`cquery`/`aquery` and their outputs, real C++,
Java and Python builtins, remote caching and execution, and `build`,
`test`, `run`, `fetch` and `clean`.

## 1. Platform-specific rule sets: Apple and Android

Bazel's first-party Apple support is native, and bb doesn't have it:

- `apple_common` is an opaque value (`starlark/opaque.rs`), apart from
  `apple_common.dotted_version`. Its platform types, `XcodeVersionConfig`,
  `AppleDynamicFrameworkInfo` and the rest are missing, so rules_apple,
  rules_swift and apple_support's Apple rules fail in analysis.
- `xcode_config`, `xcode_version` and `available_xcodes` run as Starlark
  builtins (`analysis/native.rs`), but rules' late-bound `_xcode_config` is
  a lenient stand-in (`analysis/build_ctx.rs`) and `--xcode_version_config`
  is an error (`analysis/late_bound.rs`). Bazel analyzes
  `@local_config_xcode//:host_xcodes` instead. rules_cc reads
  `_xcode_config` on every platform, which is why it's lenient.
- Bazel's native transitions, such as Apple's multi-arch splits, are opaque
  `cfg` values that bb silently treats as no transition
  (`analysis/transition.rs`): a split dependency is analyzed once, in the
  rule's own configuration, with no error.

Android:

- There is no `android_common` or any other Android builtin, and rules_android
  is untested.
- `config_feature_flag` is always at its default: transitions that set
  feature flags, which Android's resource handling uses, aren't supported
  (`analysis/condition.rs`).

Opaque values are still truthy, since starlark-rust's `to_bool` can't fail,
so `if apple_common.something:` takes the branch. Every other use of an
opaque value is an error.

## 2. Execution platforms, exec groups and remote execution

- **Exec groups.** Each exec group resolves its own toolchains and
  execution platform, and `config.exec("group")` dependencies are built
  for it. An action's `exec_group` is only checked, though: every action
  reports the rule's default execution platform, in `aquery` too. `ctx.actions.run`'s `toolchain` and `resource_set`
  are ignored. Automatic exec groups (`--incompatible_auto_exec_groups`)
  and `--use_target_platform_for_tests` are errors if turned on.
- **Remote platform.** The REAPI `Platform` of every action is empty
  (`execution/spawn.rs`): `exec_properties` and platforms'
  `exec_properties` never reach a remote executor, so remote execution on
  a platform other than the server's own doesn't work.
  `Command.execution_platform` always reports the host platform, whatever
  toolchain resolution chose.
- **Remote services are configured on the server**, not per build:
  `--remote_executor`, `--remote_cache`, `--disk_cache` and the other
  `--remote_*` options are errors unless left at their defaults.
  bb's own action cache is shared by every workspace. The server reaches
  them over `grpc://` or TLS (`grpcs://`, or no scheme), trusting
  Mozilla's root certificates; there's no way yet to trust another
  certificate or present a client certificate for them.
- **Execution requirements.** bb runs every action in a sandbox with the
  host's network. `requires-network` holds and `no-cache` is honoured, but
  `block-network` fails, and `no-sandbox`, `local`, `no-remote` and
  `no-remote-exec` still run sandboxed, which their failures point out.
  There are no spawn strategies (`--spawn_strategy`, `--strategy`). A test
  rule's `testing.ExecutionInfo` is ignored; tests take their requirements
  from tags alone.
- **Persistent workers.** There are no persistent or multiplex workers
  (`supports-workers` is passed through but unused). Java, Kotlin, Scala
  and TypeScript builds work, but their compile actions are much slower,
  and each prints the JVM's startup warnings.

## 3. Configurations

Configurations are made of native options and Starlark flags
(`config/options.rs`). Native options bb knows have Bazel's types,
defaults and parsing; a `config_setting` or transition on any other fails
rather than misbehaving. Still missing:

- `config.none()` is treated as `config.target()`
  (`starlark/rule_value.rs`).
- `--host_action_env` has no effect: exec configurations copy the target's
  `--action_env` (`analysis/transition.rs`).
- Builtins' `incompatible_*` and `experimental_*` getters on opaque values
  return `false` (`starlark/opaque.rs`), except
  `incompatible_enable_proto_toolchain_resolution`. A flag Bazel 9 turns
  on by default would be read wrongly.
- `cquery` names configurations by bb's own hash, not Bazel's checksum,
  and there are no test-trimmed configurations: where Bazel analyzes
  non-test targets without test options, bb uses the same configuration.

## 4. Actions and artifacts

- **Directories on command lines.** Command lines are built in analysis,
  before declared directories have contents, so `Args.add_all` and
  `add_joined` ignore `expand_directories`, `map_each` sees the directory
  rather than its files, and `expand_template`'s `computed_substitutions`
  can't expand one (`analysis/args.rs`). Rules that pass a tree artifact
  to `add_all` get its path instead of its files.
- **Unresolved symlinks.** `ctx.actions.symlink(target_path = ...)` fails,
  and `declare_symlink` declares an ordinary file (`analysis/actions.rs`).
- **Action conflicts** are only checked within one rule
  (`analysis/state.rs`). Two targets generating the same path, or one
  generating a file inside another's directory, aren't reported; the last
  to be written wins.
- **Toolchain Make variables.** Nothing outside the C++ rules reads
  `TemplateVariableInfo`, so `toolchains = [...]` on a genrule or Starlark
  rule doesn't define `$(PYTHON3)`, `$(JAVA)` and the like. `ctx.var` has
  only `COMPILATION_MODE`, `BINDIR`, `GENDIR`, `TARGET_CPU` and `--define`s
  (`analysis/state.rs`).
- **Param files.** Arguments only move to a param file with
  `use_always = True`; Bazel also moves them once the command line passes
  `--min_param_file_size`, so very long command lines can fail. The
  `flag_per_line` format is written like `multiline`.
- `ctx.resolve_command` is missing (`ctx.resolve_tools` works).
- `starlark_doc_extract` has no implementation, which breaks stardoc.
- `platform` targets return no providers, so `dep[PlatformInfo]` on one
  fails (`analysis/native.rs`).

## 5. Runfiles, and the environment of `run` and `test`

- **`args` and `env` aren't expanded.** `bazel run` and `bazel test` pass
  a rule's `args` and `env` through as written (`server/run.rs`,
  `execution/test.rs`); Bazel expands `$(location)`, `$(rootpath)` and
  Make variables in them, and splits `args` as a shell would.
- **`env_inherit`** (`RunEnvironmentInfo.inherited_environment`) is
  ignored, and `bazel run` applies neither it nor `environment`: the binary
  gets the client's environment and `BUILD_*_DIRECTORY`.
- **Tools' runfiles in actions** have no `_repo_mapping` or `MANIFEST`, so
  runfiles libraries under Bzlmod (Python and shell launchers used as
  tools) can fail to find files (`execution/spawn.rs`). The runfiles trees
  of tests and of `bazel run` do have them.
- `FilesToRunProvider`'s `runfiles_manifest` and `repo_mapping_manifest`
  are always `None` (`analysis/runfiles.rs`).
- Where two runfiles have the same path, bb keeps the first by label;
  Bazel prefers `root_symlinks`, then `symlinks`, then files.
- `--nobuild_runfile_links` is accepted but runfiles links are always
  built. `--noenable_runfiles` and `--nobuild_runfile_manifests` are
  errors.

## 6. The test runner

`--test_filter`, `--test_arg`, `--test_env`, `--test_timeout`,
`--test_tag_filters`, `--build_tag_filters`, `--build_tests_only`,
`--cache_test_results`, `exclusive`, timeouts by size, `test.log` and
`test.xml` work as in Bazel. Missing:

- **Sharding.** `shard_count` has no effect: nothing sets
  `TEST_TOTAL_SHARDS`, `TEST_SHARD_INDEX` or `TEST_SHARD_STATUS_FILE`, and
  `--test_sharding_strategy` isn't implemented.
- **Undeclared outputs.** `TEST_UNDECLARED_OUTPUTS_DIR`,
  `TEST_PREMATURE_EXIT_FILE` and the other output files are set, but only
  `test.xml` is collected (`execution/test.rs`): there's no
  `test.outputs/outputs.zip`, and a test that exits early with status 0
  passes.
- **Options:** `--runs_per_test`, `--flaky_test_attempts`,
  `--test_size_filters`, `--test_timeout_filters`, `--test_lang_filters`,
  `--test_summary`, `--test_verbose_timeout_warnings` (though bb prints
  Bazel's hint to use it), `--test_keep_going`, `--test_strategy` and
  `--run_under`. `--test_output=streamed` behaves like `summary`, and
  invalid values aren't rejected.
- **Coverage.** There's no `coverage` command, no LCOV collection or
  merging; `coverage_common` behaves as Bazel's does with coverage off.
- bb's action cache outlives `clean`, so tests that passed before it are
  still cached after it.

## 7. Build reporting and observability

- **Build Event Protocol.** `--build_event_json_file` and
  `--build_event_binary_file` write Bazel's events for the command line,
  options, patterns, configurations, targets and their files, test results
  and summaries, and the outcome (`server/build_events.rs`), and
  `--bes_backend` streams them to a Build Event Service over gRPC or TLS
  (`bes/`), with Bazel's lifecycle events, retries, keywords, headers and
  exit codes. `--bes_upload_mode` other than `wait_for_upload_complete`
  doesn't wait, but the next command doesn't wait for it either, and
  `--bes_proxy` and credential helpers are missing. Also missing are
  `--build_event_text_file`, structured command lines, action events,
  progress events while the command runs (bb posts one at the end), and
  source files in a target's named sets of files.
- No execution log (`--execution_log_*`), no `--profile` JSON trace and
  no `analyze-profile`, no `--explain`, and no `--announce_rc`.
- `dump` prints bb's own engine statistics; Bazel's `--action_cache`,
  `--packages`, `--rule_classes`, `--rules`, `--skyframe` and `--memory`
  are ignored.
- The counts of packages loaded and targets configured that analysis
  reports differ from Bazel's: on a small workspace, Bazel reports 5
  packages and 13 targets where bb reports 69 and 4.

## 8. Bzlmod and repositories

- **Lockfile.** `MODULE.bazel.lock` is neither read nor written, and
  `--lockfile_mode` other than `update` is an error. `extension_metadata`
  and `repo_metadata(reproducible = ...)` are accepted and ignored, so
  there are no `use_repo` fix-up warnings.
- **Options:** `--override_module`, `--override_repository` and
  `--ignore_dev_dependency` aren't implemented. Nor are credential helpers
  (`--credential_helper`); downloads use `.netrc` only.
- **Repository rules:**
  - `repository_ctx.getenv` doesn't make the repository depend on the
    variable, and `watch`, `watch_tree` and `read(watch = ...)` do nothing
    (`bzlmod/io.rs`). Only `environ`, `--repo_env` and labels in the rule's
    attributes cause a refetch.
  - `repository_ctx.rename` is missing.
  - `.7z`, `.deb` and `.ar` archives can't be extracted (`fetch/extract.rs`).
  - `execute(quiet = False)` doesn't show the output, and
    `report_progress` isn't shown.
- **Overrides:** `git_override` and `archive_override` run
  `@bazel_tools`'s own `git_repository` and `http_archive`, so they take
  every attribute Bazel does. As in Bazel, a `git_override` of a branch or
  tag is fetched once and kept until the repository is refetched.
- **Commands:** of `mod`'s subcommands only `dump_repo_mapping` works;
  `fetch` has no `--configure` or `--force`; there's no `vendor` or
  `sync`.
- **Stamping:** the status files' host, user and time are fixed, and a
  change to the volatile status reruns the actions that read it.
- `flag_alias` in MODULE.bazel has no effect.

## 9. Query tooling

`query`, `cquery` and `aquery` have all of Bazel's functions and output
formats, except:

- `cquery --output=transitions`, `--show_config_fragments`, and the
  Starlark output's `FileProvider` and `FilesToRunProvider`.
- aquery's action keys and the ids of its nested sets of inputs are bb's;
  source manifests name files in bb's execution root.
- Proto outputs leave out `transitive_configs` and
  `$rule_implementation_hash`, and give native rules every common
  attribute, where Bazel gives some, such as `alias`, fewer.
- `cquery` analyzes `_license_kind` targets in an exec configuration,
  where Bazel uses the one without options.
- `aquery` doesn't list tests' `TestRunner` actions.
- Errors for invalid regular expressions are Rust's, not Java's.
- `--infer_universe_scope` and options such as `--proto:default_values`.

## 10. Checking dependencies

Dependencies of Starlark rules are checked as Bazel checks them
(`analysis/dep_checks.rs`). Not checked:

- native rules' attributes, whose allowed files and providers bb doesn't
  know;
- aspects' implicit dependencies, and toolchains;
- output files of testonly rules
  (`--experimental_check_testonly_for_output_files`).

## 11. Commands and options

- **Commands** bb rejects: `analyze-profile`, `canonicalize-flags`,
  `config`, `coverage`, `mobile-install`, `print_action`, `sync` and
  `vendor` (`server/command.rs`).
- **`info`** has 13 keys; missing are `command_log`, `server_log`,
  `client-env`, `starlark-semantics`, `local_resources`, the heap and GC
  keys and the Java ones, and `--show_make_env`.
- **`clean --async`** runs synchronously.
- **Startup options:** `--output_base` and `--output_user_root` are
  ignored; the output base is always bb's, keyed by the workspace path.
- **Options in general.** Every option is checked against Bazel 9.3.1's
  table (`options/bazel_options.txt`). One bb doesn't implement is an
  error unless its last use leaves it at its default. Options that only
  tune Bazel's own server, such as `--local_resources` and `--watchfs`,
  are accepted and ignored. Native options whose effect bb doesn't
  implement are listed in `UNSUPPORTED_NATIVE_OPTIONS`
  (`server/build.rs`).

## 12. Host platforms

bb's actions run in Linux containers. On macOS they run in a fresh
directory of their own inputs, directly on the host, with no isolation
like Bazel's `darwin-sandbox`. Windows isn't supported, so `cmd_bat` and
`cmd_ps` are never used.

## Suggested order

1. Expanding `args` and `env`, toolchain Make variables, and `_repo_mapping`
   in tools' runfiles (sections 4 and 5): small, and common in real
   repositories.
2. Cross-target action conflicts and directory expansion in `Args`
   (section 4).
3. The remote platform (section 2).
4. Test sharding, undeclared outputs and the remaining test options
   (section 6).
6. The lockfile (section 8).
7. Apple (section 1), which needs most of the above first.
