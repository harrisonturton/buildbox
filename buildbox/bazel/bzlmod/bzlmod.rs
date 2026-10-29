//! Bzlmod: resolving the modules a workspace depends on, and the
//! repositories they and their module extensions define.
//!
//! Modules come from registries or the root module's overrides; versions
//! are chosen by minimal version selection. Repositories are fetched into
//! a directory shared by every workspace on the machine, named by what
//! defines them, so a repository is fetched once however many worktrees
//! use it.

pub mod attrs;
pub mod bootstrap;
pub mod config;
pub mod extension;
pub mod fetch_repo;
pub mod git;
pub mod graph_access;
pub mod io;
pub mod module_ctx;
pub mod module_file;
pub mod path_value;
pub mod registry;
pub mod repo_ctx;
pub mod repo_env;
pub mod repo_spec;
pub mod resolve;
pub mod resolver;
pub mod selection;
pub mod spec;
pub mod star_value;
pub mod typecheck;
pub mod version;

pub use config::BzlmodConfig;
pub use resolver::BzlmodRepositories;

pub(crate) use repo_ctx as repo_rule;
