//! Execution: running the actions analysis found, through a [`Backend`]
//! that is either this process's executor and stores, or a remote
//! execution service.
//!
//! An [`ActionKey`] runs one action and is worth the digests of its
//! outputs. Spawns go through the action cache, so an action that ran
//! before, in any workspace, isn't run again.

mod action_key;
mod backend;
mod digest;
mod inputs;
mod materialize;
mod source;
mod spawn;
mod test;

pub use action_key::{ActionKey, ActionOutputs, OutputFile, input};
pub use backend::{Backend, Runners};
pub use digest::BlobDigest;
pub use materialize::{RunfilesLayout, materialize, write_runfiles};
pub use source::{SourceDigest, SourceDigestKey, SourceDigests};
pub use spawn::{LocalActionCache, output_text};
pub use test::{TestKey, TestResult, TestStatus, next_epoch, repo_mapping};
