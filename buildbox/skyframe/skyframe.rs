//! An incremental, memoizing evaluator for build graphs, shared between
//! workspaces.
//!
//! The graph maps [`Key`]s to [`Value`]s. Each key says how to compute its
//! value from the values of other keys, which it reads through a [`Ctx`];
//! the keys it reads are its dependencies. A [`Graph`] holds one workspace's
//! results, and checks and recomputes only what changes affect.
//!
//! Every graph belongs to an [`Engine`], which holds the values themselves,
//! compressed and deduplicated, and remembers how each key was computed. A
//! workspace that needs a key another workspace has computed reuses that
//! result if the key's dependencies have the same values in both, so
//! worktrees of one repository share almost all of their work and memory.

mod cache;
mod compute;
mod ctx;
mod engine;
mod error;
mod graph;
mod key;
mod keys;
mod kinds;
mod part;
mod persist;
mod trace;

pub use cache::CacheStats;
pub use compute::Compute;
pub use ctx::Ctx;
pub use engine::{Engine, EngineConfig, EngineStats, KindRow};
pub use error::{ErrorKind, SkyError};
pub use graph::{Globals, Graph, GraphStats, Session};
pub use key::{Key, Storage, Value};
pub use keys::KeyId;
pub use part::{Part, Stored};
pub use persist::{CheckpointStats, Persistence};

pub use skycodec;
