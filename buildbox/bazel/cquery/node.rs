//! The nodes of the configured graph.

use bazel_analysis::ConfiguredTargetKey;
use bazel_config::ConfigId;
use bazel_label::{Label, RepoMappingTable};
use bazel_query::display_label;

/// A target in a configuration, or with none, as a source file has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConfiguredNode {
    pub label: Label,
    pub config: Option<ConfigId>,
}

impl ConfiguredNode {
    /// The key analysis configured the node under, if it has a
    /// configuration.
    pub fn key(&self) -> Option<ConfiguredTargetKey> {
        Some(ConfiguredTargetKey {
            label: self.label,
            config: self.config?,
        })
    }

    /// The node as `cquery` prints it: its label, then its
    /// configuration's checksum, or `null`.
    pub fn display(&self, mapping: &RepoMappingTable) -> String {
        format!("{} ({})", display_label(self.label, mapping), config_name(self.config))
    }
}

/// A configuration as `cquery` names it: by its checksum, or `null`.
pub fn config_name(config: Option<ConfigId>) -> String {
    match config {
        Some(config) => config.get().checksum.clone(),
        None => "null".to_string(),
    }
}
