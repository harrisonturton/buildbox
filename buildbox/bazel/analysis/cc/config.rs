//! Toolchain configurations, as `cc_common.create_cc_toolchain_config_info`
//! takes them.

use super::features::{ActionConfig, EnvEntry, EnvSet, Feature, FlagGroup, FlagSet, Tool, ToolchainFeatures, WithFeatureSet};
use starlark::values::{Heap, Value};
use std::collections::BTreeMap;

/// What a C++ toolchain is, and how its actions are configured.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CcToolchainConfig {
    pub toolchain_identifier: String,
    pub host_system_name: String,
    pub target_system_name: String,
    pub target_cpu: String,
    pub target_libc: String,
    pub compiler: String,
    pub abi_version: String,
    pub abi_libc_version: String,
    pub builtin_sysroot: Option<String>,
    pub cxx_builtin_include_directories: Vec<String>,
    /// Tools by name, such as `gcc` and `ar`, as paths absolute or relative
    /// to the execution root.
    pub tool_paths: BTreeMap<String, String>,
    pub make_variables: Vec<(String, String)>,
    /// File name prefixes and extensions by artifact category, such as
    /// `static_library`.
    pub artifact_name_patterns: BTreeMap<String, (String, String)>,
    pub features: std::sync::Arc<ToolchainFeatures>,
}

/// The actions Bazel configures from `tool_paths` when a toolchain doesn't
/// configure them itself: each action, its tool, and the features it
/// implies.
const LEGACY_ACTIONS: &[(&[&str], &str, &[&str])] = &[
    (
        &["c-compile", "c++-compile", "c++-header-parsing", "c++-module-compile", "c++-module-codegen", "linkstamp-compile", "lto-backend", "clif-match", "preprocess-assemble"],
        "gcc",
        &["legacy_compile_flags", "user_compile_flags", "sysroot", "unfiltered_compile_flags", "compiler_input_flags", "compiler_output_flags"],
    ),
    (&["assemble"], "gcc", &["user_compile_flags", "sysroot", "unfiltered_compile_flags", "compiler_input_flags", "compiler_output_flags"]),
    (
        &["c++-link-executable", "lto-index-for-executable"],
        "gcc",
        &[
            "symbol_counts",
            "strip_debug_symbols",
            "linkstamps",
            "output_execpath_flags",
            "runtime_library_search_directories",
            "library_search_directories",
            "libraries_to_link",
            "force_pic_flags",
            "user_link_flags",
            "legacy_link_flags",
            "linker_param_file",
            "fission_support",
            "sysroot",
        ],
    ),
    (
        &["c++-link-dynamic-library", "c++-link-nodeps-dynamic-library", "lto-index-for-dynamic-library", "lto-index-for-nodeps-dynamic-library"],
        "gcc",
        &[
            "build_interface_libraries",
            "dynamic_library_linker_tool",
            "symbol_counts",
            "strip_debug_symbols",
            "shared_flag",
            "linkstamps",
            "output_execpath_flags",
            "runtime_library_search_directories",
            "library_search_directories",
            "libraries_to_link",
            "user_link_flags",
            "legacy_link_flags",
            "linker_param_file",
            "fission_support",
            "sysroot",
        ],
    ),
    (&["c++-link-static-library"], "ar", &["archiver_flags", "linker_param_file"]),
    (&["strip"], "strip", &[]),
    (&["objcopy_embed_data"], "objcopy", &[]),
    (&["validate-static-library"], "validate_static_library", &[]),
    (&["c++-module-deps-scanning"], "cpp-module-deps-scanner", &[]),
];

impl CcToolchainConfig {
    /// Add the action configs Bazel makes from `tool_paths` for actions the
    /// toolchain leaves unconfigured, unless it has `no_legacy_features`.
    pub fn add_legacy_action_configs(features: &mut ToolchainFeatures, tool_paths: &BTreeMap<String, String>) {
        if features.features.iter().any(|feature| feature.name == "no_legacy_features") {
            return;
        }
        // Bazel puts the legacy flags of a toolchain first on command lines.
        for name in ["default_compile_flags", "legacy_compile_flags"] {
            if let Some(position) = features.features.iter().position(|feature| feature.name == name) {
                let feature = features.features.remove(position);
                features.features.insert(0, feature);
            }
        }
        for (actions, tool, implies) in LEGACY_ACTIONS {
            let Some(path) = tool_paths.get(*tool) else {
                continue;
            };
            for action in *actions {
                if features.action_configs.iter().any(|config| config.action_name == *action) {
                    continue;
                }
                features.action_configs.push(ActionConfig {
                    action_name: action.to_string(),
                    enabled: true,
                    tools: vec![Tool {
                        path: path.clone(),
                        ..Default::default()
                    }],
                    flag_sets: Vec::new(),
                    implies: implies.iter().map(|feature| feature.to_string()).collect(),
                });
            }
        }
    }

    /// The path of a tool by name, such as `ar`.
    pub fn tool_path(&self, name: &str) -> Option<&str> {
        self.tool_paths.get(name).map(String::as_str)
    }
}

/// Reads the structs `cc_toolchain_config_lib.bzl` makes, which may be
/// providers or plain structs.
pub struct Reader<'v> {
    pub heap: Heap<'v>,
}

impl<'v> Reader<'v> {
    fn field(&self, value: Value<'v>, name: &str) -> Option<Value<'v>> {
        value.get_attr(name, self.heap).ok().flatten().filter(|value| !value.is_none())
    }

    fn string(&self, value: Value<'v>, name: &str) -> Option<String> {
        self.field(value, name).and_then(|value| value.unpack_str().map(str::to_string)).filter(|value| !value.is_empty())
    }

    fn bool(&self, value: Value<'v>, name: &str) -> bool {
        self.field(value, name).is_some_and(|value| value.to_bool())
    }

    fn list(&self, value: Value<'v>, name: &str) -> Vec<Value<'v>> {
        self.field(value, name).and_then(bazel_starlark::convert::items).unwrap_or_default()
    }

    fn strings(&self, value: Value<'v>, name: &str) -> Vec<String> {
        self.list(value, name).into_iter().filter_map(|item| item.unpack_str().map(str::to_string)).collect()
    }

    pub fn feature(&self, value: Value<'v>) -> Feature {
        Feature {
            name: self.string(value, "name").unwrap_or_default(),
            enabled: self.bool(value, "enabled"),
            flag_sets: self.list(value, "flag_sets").into_iter().map(|set| self.flag_set(set)).collect(),
            env_sets: self.list(value, "env_sets").into_iter().map(|set| self.env_set(set)).collect(),
            requires: self.list(value, "requires").into_iter().map(|set| self.strings(set, "features")).collect(),
            implies: self.strings(value, "implies"),
            provides: self.strings(value, "provides"),
        }
    }

    pub fn action_config(&self, value: Value<'v>, package_dir: &str) -> ActionConfig {
        let action_name = self.string(value, "action_name").unwrap_or_default();
        let mut flag_sets: Vec<FlagSet> = self.list(value, "flag_sets").into_iter().map(|set| self.flag_set(set)).collect();
        for flag_set in &mut flag_sets {
            flag_set.actions = vec![action_name.clone()];
        }
        ActionConfig {
            enabled: self.bool(value, "enabled"),
            tools: self.list(value, "tools").into_iter().map(|tool| self.tool(tool, package_dir)).collect(),
            flag_sets,
            implies: self.strings(value, "implies"),
            action_name,
        }
    }

    fn tool(&self, value: Value<'v>, package_dir: &str) -> Tool {
        let path = match self.field(value, "tool").and_then(crate::file_value::FileValue::artifact) {
            Some(artifact) => artifact.exec_path.to_string(),
            None => resolve_tool_path(&self.string(value, "path").unwrap_or_default(), package_dir),
        };
        Tool {
            path,
            with_features: self.list(value, "with_features").into_iter().map(|set| self.with_feature_set(set)).collect(),
            execution_requirements: self.strings(value, "execution_requirements"),
        }
    }

    fn flag_set(&self, value: Value<'v>) -> FlagSet {
        FlagSet {
            actions: self.strings(value, "actions"),
            with_features: self.list(value, "with_features").into_iter().map(|set| self.with_feature_set(set)).collect(),
            expand_if_all_available: self.strings(value, "expand_if_all_available"),
            flag_groups: self.list(value, "flag_groups").into_iter().map(|group| self.flag_group(group)).collect(),
        }
    }

    fn with_feature_set(&self, value: Value<'v>) -> WithFeatureSet {
        WithFeatureSet {
            features: self.strings(value, "features"),
            not_features: self.strings(value, "not_features"),
        }
    }

    fn flag_group(&self, value: Value<'v>) -> FlagGroup {
        let expand_if_equal = self.field(value, "expand_if_equal").and_then(|equal| Some((self.string(equal, "name")?, self.string(equal, "value").unwrap_or_default())));
        FlagGroup {
            flags: self.strings(value, "flags"),
            flag_groups: self.list(value, "flag_groups").into_iter().map(|group| self.flag_group(group)).collect(),
            iterate_over: self.string(value, "iterate_over"),
            expand_if_available: self.string(value, "expand_if_available"),
            expand_if_not_available: self.string(value, "expand_if_not_available"),
            expand_if_true: self.string(value, "expand_if_true"),
            expand_if_false: self.string(value, "expand_if_false"),
            expand_if_equal,
        }
    }

    fn env_set(&self, value: Value<'v>) -> EnvSet {
        EnvSet {
            actions: self.strings(value, "actions"),
            with_features: self.list(value, "with_features").into_iter().map(|set| self.with_feature_set(set)).collect(),
            env_entries: self
                .list(value, "env_entries")
                .into_iter()
                .map(|entry| EnvEntry {
                    key: self.string(entry, "key").unwrap_or_default(),
                    value: self.field(entry, "value").and_then(|value| value.unpack_str().map(str::to_string)).unwrap_or_default(),
                    expand_if_available: self.string(entry, "expand_if_available"),
                })
                .collect(),
        }
    }

    /// `tool_paths`: a list of `tool_path` structs, or a dict, as rules_cc's
    /// newer toolchain configs give.
    pub fn tool_paths(&self, value: Option<Value<'v>>, package_dir: &str) -> BTreeMap<String, String> {
        let mut paths = BTreeMap::new();
        let Some(value) = value.filter(|value| !value.is_none()) else {
            return paths;
        };
        if let Some(dict) = starlark::values::dict::DictRef::from_value(value) {
            for (name, path) in dict.iter() {
                if let (Some(name), Some(path)) = (name.unpack_str(), path.unpack_str()) {
                    paths.insert(name.to_string(), resolve_tool_path(path, package_dir));
                }
            }
            return paths;
        }
        for item in bazel_starlark::convert::items(value).unwrap_or_default() {
            if let (Some(name), Some(path)) = (self.string(item, "name"), self.string(item, "path")) {
                paths.insert(name, resolve_tool_path(&path, package_dir));
            }
        }
        paths
    }

    pub fn make_variables(&self, value: Option<Value<'v>>) -> Vec<(String, String)> {
        let items = value.and_then(bazel_starlark::convert::items).unwrap_or_default();
        items.into_iter().filter_map(|item| Some((self.string(item, "name")?, self.string(item, "value").unwrap_or_default()))).collect()
    }

    pub fn artifact_name_patterns(&self, value: Option<Value<'v>>) -> BTreeMap<String, (String, String)> {
        let items = value.and_then(bazel_starlark::convert::items).unwrap_or_default();
        items
            .into_iter()
            .filter_map(|item| {
                let category = self.string(item, "category_name")?;
                Some((category, (self.string(item, "prefix").unwrap_or_default(), self.string(item, "extension").unwrap_or_default())))
            })
            .collect()
    }
}

/// A tool path relative to the toolchain's package, as an execution root
/// path; absolute paths stay as they are.
fn resolve_tool_path(path: &str, package_dir: &str) -> String {
    if path.starts_with('/') || package_dir.is_empty() {
        return path.to_string();
    }
    format!("{package_dir}/{path}")
}
