//! Analysis: running rules' implementations in configurations, to find the
//! actions that build their files.
//!
//! A [`ConfiguredTargetKey`] analyzes a target in a configuration: its
//! attributes' `select()`s resolved, its dependencies analyzed, and its
//! rule's implementation run with a `ctx`. The result is the target's
//! providers, which its dependents see, and the actions its rule registered.

mod action;
mod action_value;
pub mod aspect;
mod actions;
mod args;
mod artifact;
mod attrs;
mod build_ctx;
pub mod cc;
mod condition;
pub mod conflicts;
mod configured;
mod coverage;
mod dep_checks;
mod exec_groups;
mod expand;
mod failure;
mod file_value;
mod fragments;
mod inputs;
mod java;
mod late_bound;
mod native;
mod package_spec;
pub mod paging;
mod platform;
mod platform_info;
mod propagation;
mod providers;
mod python;
mod repo_mapping;
mod rule_ctx;
mod shared;
mod shell;
mod runfiles;
pub mod settings;
mod state;
mod target_value;
mod toolchains;
pub mod transition;
pub mod workspace_status;

pub use action::{Action, ActionKind, ParamFile, RunfilesTree, Spawn, StoredAction, environment};
pub use args::param_file_arguments;
pub use inputs::{InputSet, KnownSets};
pub use artifact::{Artifact, name_path, package_dir, parse as parse_path, repo_dir, runfiles_repo_name};
pub use attrs::{ConfiguredAttrs, ConfiguredAttrsKey};
pub use transition::native_value;
pub use configured::{ConfiguredDep, ConfiguredTarget, ConfiguredTargetKey, Executable, Incompatibility, Providers, RunEnvironment};
pub use file_value::FileValue;
pub use platform::{Platform, PlatformKey};
pub use repo_mapping::repo_mapping_manifest;
pub use runfiles::{FilesToRun, Runfiles};
pub use state::runfiles_tree;
pub use target_value::{DEFAULT_GROUP, DEFAULT_INFO, TargetValue, depset_artifacts};

/// Add the natives analysis provides to Starlark: the C++ and coverage
/// APIs, and how subrules find their attributes. Call before any file is
/// evaluated.
pub fn register() {
    cc::register();
    java::register();
    bazel_starlark::register_builtins_internal(python::register_builtins_internal);
    bazel_starlark::register_bzl_extension(coverage::register);
    build_ctx::register();
}
