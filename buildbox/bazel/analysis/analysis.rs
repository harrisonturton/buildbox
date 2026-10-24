//! Analysis: running rules' implementations in configurations, to find the
//! actions that build their files.
//!
//! A [`ConfiguredTargetKey`] analyzes a target in a configuration: its
//! attributes' `select()`s resolved, its dependencies analyzed, and its
//! rule's implementation run with a `ctx`. The result is the target's
//! providers, which its dependents see, and the actions its rule registered.

mod action;
mod actions;
mod args;
mod artifact;
mod attrs;
mod build_ctx;
mod condition;
mod configured;
mod expand;
mod file_value;
mod native;
mod platform;
mod providers;
mod rule_ctx;
mod runfiles;
mod state;
mod target_value;
mod toolchains;

pub use action::{Action, ActionKind, RunfilesTree, Spawn};
pub use artifact::{Artifact, name_path, package_dir, repo_dir, runfiles_repo_name};
pub use configured::{ConfiguredTarget, ConfiguredTargetKey};
pub use file_value::FileValue;
pub use platform::{Platform, PlatformKey};
pub use runfiles::{FilesToRun, Runfiles};
pub use state::runfiles_tree;
pub use target_value::{DEFAULT_INFO, TargetValue};
