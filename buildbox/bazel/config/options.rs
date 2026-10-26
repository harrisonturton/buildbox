//! Native options: those Bazel's configuration fragments declare, which
//! make up a configuration along with Starlark flags.
//!
//! Each native option bb knows is in [`NATIVE_OPTIONS`], with its type and
//! default. [`BuildOptions`] holds only the values that differ from the
//! defaults, so adding an option doesn't change the configurations that
//! don't set it.

use bazel_label::Label;
use skycodec::{Decode, Encode};
use std::sync::LazyLock;

/// The kind of value a native option has, and how the command line gives
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionType {
    Bool,
    Int,
    String,
    /// `auto`, `yes` or `no`, which `true`, `1`, `false` and `0` also give.
    TriState,
    /// A label, or none.
    Label,
    /// A list of strings: one for each use of the option.
    Strings,
    /// A list of strings, split at commas.
    CommaStrings,
    /// A list of labels, split at commas.
    Labels,
    /// `name=value` pairs, such as `--define`'s.
    Assignments,
}

/// What an option is when the command line doesn't give it.
#[derive(Clone, Copy, Debug)]
pub enum OptionDefault {
    /// The value the command line text gives, such as `fastbuild`. For
    /// lists, an empty text is the empty list.
    Text(&'static str),
    /// No value, as Bazel's `null` defaults: neither an empty string nor an
    /// empty list.
    Null,
    /// This machine's CPU, as Bazel's `--cpu` and `--host_cpu` default to.
    HostCpu,
}

/// A native option.
#[derive(Debug)]
pub struct NativeOption {
    pub name: &'static str,
    pub ty: OptionType,
    pub default: OptionDefault,
    /// For lists: whether each use of the option adds to the list, rather
    /// than replacing it.
    pub multiple: bool,
    /// For strings: the values the option allows, if it restricts them, in
    /// the order Bazel lists them, and what Bazel calls them.
    pub allowed: &'static [&'static str],
    pub allowed_name: &'static str,
    /// Whether Bazel prints the option's values in upper case, as it does
    /// Java enums that don't name their values.
    pub prints_upper: bool,
}

/// The value of a native option.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum OptionValue {
    /// The value of an option whose default is [`OptionDefault::Null`].
    Null,
    Bool(bool),
    Int(i64),
    String(String),
    Label(Label),
    Strings(Vec<String>),
    Labels(Vec<Label>),
    Assignments(Vec<(String, String)>),
}

/// An option that doesn't exist, or a value it can't have.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OptionError {
    #[error("unknown option: '{0}'")]
    Unknown(String),

    #[error("{0}")]
    Invalid(String),
}

/// Parses a label given on the command line or in a `config_setting`,
/// relative to the main repository.
pub type LabelParser<'a> = &'a (dyn Fn(&str) -> Result<Label, String> + Sync);

const fn option(name: &'static str, ty: OptionType, default: OptionDefault) -> NativeOption {
    NativeOption {
        name,
        ty,
        default,
        multiple: false,
        allowed: &[],
        allowed_name: "",
        prints_upper: false,
    }
}

const fn multiple(name: &'static str, ty: OptionType) -> NativeOption {
    NativeOption {
        name,
        ty,
        default: OptionDefault::Null,
        multiple: true,
        allowed: &[],
        allowed_name: "",
        prints_upper: false,
    }
}

const fn choice(name: &'static str, default: &'static str, allowed_name: &'static str, allowed: &'static [&'static str]) -> NativeOption {
    NativeOption {
        name,
        ty: OptionType::String,
        default: OptionDefault::Text(default),
        multiple: false,
        allowed,
        allowed_name,
        prints_upper: false,
    }
}

/// A [`choice`] whose values Bazel prints in upper case.
const fn upper_choice(name: &'static str, default: &'static str, allowed_name: &'static str, allowed: &'static [&'static str]) -> NativeOption {
    NativeOption {
        prints_upper: true,
        ..choice(name, default, allowed_name, allowed)
    }
}

const STRICT_DEPS: &str = "strict dependency checking level";
const STRICT_DEPS_MODES: &[&str] = &["off", "warn", "error", "strict", "default"];

/// The native options bb knows, sorted by name, with Bazel 9.3.1's types
/// and defaults: those `config_setting`s match in the rule sets bb has met,
/// those bb reads, and every one Bazel's exec transition reads or writes.
/// Two are bb's own, which Bazel keeps in targets' keys instead:
/// `toolchain execution platform`, the execution platform of the rule a
/// toolchain target is analyzed for, and `applied aspects`, the aspects
/// whose application to a target a key names.
pub static NATIVE_OPTIONS: &[NativeOption] = {
    use OptionDefault::*;
    use OptionType::*;
    &[        option("Android configuration distinguisher", String, Text("main")),
        multiple("action_env", Strings),
        option("all feature flag values are present (internal)", Bool, Text("true")),
        option("allow_analysis_failures", Bool, Text("false")),
        option("allow_unresolved_symlinks", Bool, Text("true")),
        option("android_databinding_use_androidx", Bool, Text("true")),
        option("android_databinding_use_v3_4_args", Bool, Text("true")),
        option("android_manifest_merger", String, Text("android")),
        option("android_manifest_merger_order", String, Text("alphabetical")),
        option("apple configuration distinguisher", String, Text("unknown")),
        option("apple_platform_type", String, Text("macos")),
        multiple("applied aspects", Strings),
        option("archived_tree_artifact_mnemonics_filter", String, Text("-.*")),
        option("break_build_on_parallel_dex2oat_failure", Bool, Text("false")),
        option("build_python_zip", TriState, Text("auto")),
        option("build_runfile_links", Bool, Text("true")),
        option("build_runfile_manifests", Bool, Text("true")),
        option("bytecode_optimization_pass_actions", Int, Text("1")),
        option("bytecode_optimizers", String, Text("Proguard")),
        option("cc_dotd_files", Bool, Text("true")),
        option("cc_include_scanning", Bool, Text("false")),
        option("cc_proto_library_header_suffixes", CommaStrings, Text(".pb.h")),
        option("cc_proto_library_source_suffixes", CommaStrings, Text(".pb.cc")),
        option("check_licenses", Bool, Text("false")),
        option("check_visibility", Bool, Text("true")),
        option("collect_code_coverage", Bool, Text("false")),
        choice("compilation_mode", "fastbuild", "compilation mode", &["fastbuild", "dbg", "opt"]),
        option("compiler", String, Null),
        multiple("conlyopt", Strings),
        multiple("copt", Strings),
        option("cpu", String, HostCpu),
        option("crosstool_top", Label, Null),
        multiple("cxxopt", Strings),
        multiple("define", Assignments),
        option("desugar_for_android", Bool, Text("true")),
        option("desugar_java8_libs", Bool, Text("false")),
        option("dexopts_supported_in_dexmerger", CommaStrings, Text("--minimal-main-dex,--set-max-idx-number")),
        option("dexopts_supported_in_dexsharder", CommaStrings, Text("--minimal-main-dex")),
        option("dexopts_supported_in_incremental_dexing", CommaStrings, Text("--no-optimize,--no-locals")),
        upper_choice("dynamic_mode", "default", "dynamic mode", &["off", "default", "fully"]),
        option("enable_runfiles", TriState, Text("auto")),
        option("enforce_constraints", Bool, Text("true")),
        option("enforce_proguard_file_extension", Bool, Text("false")),
        option("enforce_transitive_configs_for_config_feature_flag", Bool, Text("false")),
        multiple("experimental_action_listener", Labels),
        option("experimental_add_test_support_to_compile_time_deps", Bool, Text("true")),
        option("experimental_allow_android_library_deps_without_srcs", Bool, Text("false")),
        option("experimental_allow_map_directory", Bool, Text("true")),
        option("experimental_always_filter_duplicate_classes_from_android_test", Bool, Text("false")),
        option("experimental_android_assume_minsdkversion", Bool, Text("false")),
        option("experimental_android_compress_java_resources", Bool, Text("false")),
        option("experimental_android_databinding_v2", Bool, Text("true")),
        option("experimental_android_library_exports_manifest_default", Bool, Text("false")),
        option("experimental_android_resource_cycle_shrinking", Bool, Text("false")),
        option("experimental_android_resource_name_obfuscation", Bool, Text("false")),
        option("experimental_android_resource_path_shortening", Bool, Text("false")),
        option("experimental_android_resource_shrinking", Bool, Text("false")),
        option("experimental_android_rewrite_dexes_with_rex", Bool, Text("false")),
        option("experimental_android_use_parallel_dex2oat", Bool, Text("false")),
        option("experimental_cc_implementation_deps", Bool, Text("true")),
        option("experimental_check_desugar_deps", Bool, Text("true")),
        option("experimental_collect_code_coverage_for_generated_files", Bool, Text("false")),
        option("experimental_cpp_compile_resource_estimation", Bool, Text("false")),
        option("experimental_cpp_modules", Bool, Text("false")),
        option("experimental_debug_selects_always_succeed", Bool, Text("false")),
        option("experimental_disable_instrumentation_manifest_merge", Bool, Text("false")),
        option("experimental_enable_jspecify", Bool, Text("true")),
        option("experimental_enforce_transitive_visibility", Bool, Text("false")),
        option("experimental_exclude_defines_from_exec_config", Bool, Text("false")),
        option("experimental_exec_config", String, Text("@_builtins//:common/builtin_exec_platforms.bzl%bazel_exec_transition")),
        option("experimental_exec_configuration_distinguisher", String, Text("off")),
        option("experimental_extended_sanity_checks", Bool, Text("false")),
        option("experimental_filter_library_jar_with_program_jar", Bool, Text("false")),
        option("experimental_filter_r_jars_from_android_test", Bool, Text("false")),
        option("experimental_fix_deps_tool", String, Text("add_dep")),
        option("experimental_generate_llvm_lcov", Bool, Text("false")),
        option("experimental_get_android_java_resources_from_optimized_jar", Bool, Text("false")),
        option("experimental_include_xcode_execution_requirements", Bool, Text("false")),
        option("experimental_incremental_dexing_after_proguard", Int, Text("50")),
        option("experimental_incremental_dexing_after_proguard_by_default", Bool, Text("true")),
        option("experimental_inmemory_dotd_files", Bool, Text("true")),
        option("experimental_inmemory_jdeps_files", Bool, Text("true")),
        option("experimental_java_classpath", String, Text("bazel")),
        option("experimental_java_test_auto_create_deploy_jar", Bool, Text("false")),
        option("experimental_link_static_libraries_once", Bool, Text("true")),
        option("experimental_objc_provider_from_linked", Bool, Text("false")),
        option("experimental_omit_resources_info_provider_from_android_binary", Bool, Text("false")),
        option("experimental_omitfp", Bool, Text("false")),
        option("experimental_one_version_enforcement", String, Text("off")),
        option("experimental_one_version_enforcement_use_transitive_jars_for_binary_under_test", Bool, Text("false")),
        option("experimental_output_directory_naming_scheme", String, Text("diff_against_dynamic_baseline")),
        option("experimental_output_paths", String, Text("off")),
        option("experimental_persistent_aar_extractor", Bool, Text("false")),
        option("experimental_platform_in_output_dir", TriState, Text("auto")),
        option("experimental_prefer_mutual_xcode", Bool, Text("true")),
        multiple("experimental_propagate_custom_flag", Strings),
        option("experimental_proto_descriptor_sets_include_source_info", Bool, Text("false")),
        option("experimental_py_binaries_include_label", Bool, Text("false")),
        option("experimental_python_import_all_repositories", Bool, Text("true")),
        option("experimental_remotable_source_manifests", Bool, Text("false")),
        option("experimental_remove_r_classes_from_instrumentation_test_jar", Bool, Text("true")),
        option("experimental_run_android_lint_on_java_rules", Bool, Text("false")),
        option("experimental_save_feature_state", Bool, Text("false")),
        option("experimental_strict_fileset_output", Bool, Text("false")),
        option("experimental_strict_java_deps", String, Text("default")),
        option("experimental_throttle_action_cache_check", Bool, Text("true")),
        option("experimental_turbine_annotation_processing", Bool, Text("false")),
        option("experimental_unsupported_and_brittle_include_scanning", Bool, Text("false")),
        option("experimental_use_cpp_compile_action_args_params_file", Bool, Text("false")),
        option("experimental_use_dex_splitter_for_incremental_dexing", Bool, Text("true")),
        option("experimental_use_llvm_covmap", Bool, Text("false")),
        option("experimental_use_platforms_in_output_dir_legacy_heuristic", Bool, Text("true")),
        option("experimental_use_rtxt_from_merged_resources", Bool, Text("false")),
        option("experimental_writable_outputs", Bool, Text("false")),
        option("extra_execution_platforms", CommaStrings, Text("")),
        multiple("extra_toolchains", CommaStrings),
        multiple("features", Strings),
        option("fission", String, Text("no")),
        option("force_pic", Bool, Text("false")),
        option("grte_top", Label, Null),
        multiple("host_action_env", Strings),
        option("host_compilation_mode", String, Text("opt")),
        option("host_compiler", String, Null),
        multiple("host_conlyopt", Strings),
        multiple("host_copt", Strings),
        option("host_cpu", String, HostCpu),
        multiple("host_cxxopt", Strings),
        multiple("host_features", Strings),
        option("host_grte_top", Label, Null),
        option("host_java_launcher", Label, Null),
        multiple("host_javacopt", Strings),
        multiple("host_jvmopt", Strings),
        multiple("host_linkopt", Strings),
        option("host_macos_minimum_os", String, Null),
        multiple("host_per_file_copt", Strings),
        option("host_platform", Label, Text("@@platforms//host:host")),
        option("include_config_fragments_provider", String, Text("off")),
        option("incompatible_always_include_files_in_data", Bool, Text("true")),
        option("incompatible_auto_exec_groups", Bool, Text("false")),
        option("incompatible_avoid_hardcoded_objc_compilation_flags", Bool, Text("true")),
        option("incompatible_bazel_test_exec_run_under", Bool, Text("true")),
        option("incompatible_bep_cpu_from_platform", Bool, Text("false")),
        option("incompatible_builtin_objc_strip_action", Bool, Text("true")),
        option("incompatible_check_testonly_for_output_files", Bool, Text("false")),
        option("incompatible_compact_repo_mapping_manifest", Bool, Text("true")),
        option("incompatible_default_to_explicit_init_py", Bool, Text("false")),
        option("incompatible_disable_legacy_cc_provider", Bool, Text("true")),
        option("incompatible_disable_native_android_rules", Bool, Text("false")),
        option("incompatible_disable_nocopts", Bool, Text("true")),
        option("incompatible_disallow_java_import_exports", Bool, Text("false")),
        option("incompatible_disallow_sdk_frameworks_attributes", Bool, Text("false")),
        option("incompatible_dont_enable_host_nonhost_crosstool_features", Bool, Text("true")),
        option("incompatible_enable_apple_toolchain_resolution", Bool, Text("false")),
        option("incompatible_enable_cc_toolchain_resolution", Bool, Text("true")),
        option("incompatible_exclude_starlark_flags_from_exec_config", Bool, Text("false")),
        option("incompatible_filegroup_runfiles_for_data", Bool, Text("true")),
        option("incompatible_limit_platforms_in_output_dir_to", Labels, Text("")),
        option("incompatible_make_thinlto_command_lines_standalone", Bool, Text("true")),
        option("incompatible_merge_genfiles_directory", Bool, Text("true")),
        option("incompatible_modify_execution_info_additive", Bool, Text("true")),
        option("incompatible_multi_release_deploy_jars", Bool, Text("true")),
        option("incompatible_objc_alwayslink_by_default", Bool, Text("false")),
        option("incompatible_python_disallow_native_rules", Bool, Text("false")),
        option("incompatible_remove_ctx_android_fragment", Bool, Text("false")),
        option("incompatible_remove_ctx_bazel_py_fragment", Bool, Text("true")),
        option("incompatible_remove_ctx_py_fragment", Bool, Text("true")),
        option("incompatible_remove_legacy_whole_archive", Bool, Text("true")),
        option("incompatible_require_ctx_in_configure_features", Bool, Text("true")),
        option("incompatible_strict_action_env", Bool, Text("true")),
        option("incompatible_strip_executable_safely", Bool, Text("false")),
        option("incompatible_use_cpp_compile_header_mnemonic", Bool, Text("false")),
        option("incompatible_use_specific_tool_files", Bool, Text("true")),
        option("incompatible_use_toolchain_resolution_for_java_rules", Bool, Text("true")),
        option("incompatible_validate_top_level_header_inclusions", Bool, Text("true")),
        option("incremental_dexing", Bool, Text("true")),
        option("internal_persistent_busybox_tools", Bool, Text("false")),
        option("internal_persistent_multiplex_busybox_tools", Bool, Text("false")),
        multiple("ios_multi_cpus", CommaStrings),
        option("ios_sdk_version", String, Null),
        option("is exec configuration", Bool, Text("false")),
        option("java_deps", Bool, Text("true")),
        option("java_header_compilation", Bool, Text("true")),
        option("java_language_version", String, Text("")),
        option("java_launcher", Label, Null),
        option("java_runtime_version", String, Text("local_jdk")),
        multiple("javacopt", Strings),
        multiple("jvmopt", Strings),
        multiple("linkopt", Strings),
        multiple("macos_cpus", CommaStrings),
        option("macos_minimum_os", String, Null),
        option("macos_sdk_version", String, Null),
        option("non_incremental_per_target_dexopts", CommaStrings, Text("--positions")),
        option("objc_use_dotd_pruning", Bool, Text("true")),
        option("one_version_enforcement_on_java_tests", Bool, Text("true")),
        multiple("per_file_copt", Strings),
        option("platform_mappings", String, Text("")),
        option("platforms", Labels, Text("")),
        option("proguard_top", Label, Null),
        option("proto_compiler", Label, Text("@@bazel_tools//tools/proto:protoc")),
        option("proto_toolchain_for_cc", Label, Text("@@bazel_tools//tools/proto:cc_toolchain")),
        option("proto_toolchain_for_j2objc", Label, Text("@@bazel_tools//tools/j2objc:j2objc_proto_toolchain")),
        option("proto_toolchain_for_java", Label, Text("@@bazel_tools//tools/proto:java_toolchain")),
        option("proto_toolchain_for_javalite", Label, Text("@@bazel_tools//tools/proto:javalite_toolchain")),
        multiple("protocopt", Strings),
        option("python_native_rules_allowlist", Label, Null),
        option("python_path", String, Null),
        option("shell_executable", String, Null),
        option("split_bytecode_optimization_pass", Bool, Text("false")),
        option("stamp", Bool, Text("false")),
        option("start_end_lib", Bool, Text("true")),
        option("strict_filesets", Bool, Text("false")),
        upper_choice("strict_proto_deps", "error", STRICT_DEPS, STRICT_DEPS_MODES),
        upper_choice("strict_public_imports", "off", STRICT_DEPS, STRICT_DEPS_MODES),
        option("strict_system_includes", Bool, Text("false")),
        choice("strip", "sometimes", "strip mode", &["always", "sometimes", "never"]),
        option("tool_java_language_version", String, Text("")),
        option("tool_java_runtime_version", String, Text("remotejdk_11")),
        option("toolchain execution platform", Label, Null),
        option("toolchain_resolution_debug", String, Text("-.*")),
        option("tvos_sdk_version", String, Null),
        option("use_ijars", Bool, Text("true")),
        option("use_platforms_in_apple_crosstool_transition", Bool, Text("false")),
        option("watchos_sdk_version", String, Null),
        option("xcode_version", String, Null),
        option("xcode_version_config", Label, Text("@@bazel_tools//tools/cpp:host_xcodes")),
    ]
};

/// The native options the command line can give: all but Bazel's internal
/// ones, whose names have spaces.
pub fn command_line_options() -> impl Iterator<Item = &'static NativeOption> {
    NATIVE_OPTIONS.iter().filter(|option| !option.name.contains(' '))
}

/// The default of every native option, in [`NATIVE_OPTIONS`]' order.
static DEFAULTS: LazyLock<Vec<OptionValue>> = LazyLock::new(|| NATIVE_OPTIONS.iter().map(default_value).collect());

/// The native option named `name`.
pub fn native_option(name: &str) -> Option<&'static NativeOption> {
    let index = NATIVE_OPTIONS.binary_search_by(|option| option.name.cmp(name)).ok()?;
    Some(&NATIVE_OPTIONS[index])
}

/// An option's default value.
pub fn default_of(option: &NativeOption) -> &'static OptionValue {
    let index = NATIVE_OPTIONS.binary_search_by(|candidate| candidate.name.cmp(option.name)).expect("a native option");
    &DEFAULTS[index]
}

fn default_value(option: &NativeOption) -> OptionValue {
    match option.default {
        OptionDefault::Null => null_value(option.ty),
        OptionDefault::HostCpu => OptionValue::String(crate::host_cpu().to_string()),
        OptionDefault::Text(text) => {
            let canonical = |text: &str| Label::parse_canonical(text).map_err(|error| error.to_string());
            option.parse(text, &canonical).expect("native options' defaults parse")
        }
    }
}

/// What an option with a `null` default holds: nothing, or for lists, the
/// empty list, since Bazel's lists start empty.
fn null_value(ty: OptionType) -> OptionValue {
    match ty {
        OptionType::Strings | OptionType::CommaStrings => OptionValue::Strings(Vec::new()),
        OptionType::Labels => OptionValue::Labels(Vec::new()),
        OptionType::Assignments => OptionValue::Assignments(Vec::new()),
        _ => OptionValue::Null,
    }
}

impl NativeOption {
    /// A value as Bazel's Java prints it, which output directories' names
    /// hash, or `None` for no value.
    pub fn java_text(&self, value: &OptionValue) -> Option<String> {
        let list = |items: Vec<String>| format!("[{}]", items.join(", "));
        Some(match value {
            OptionValue::Null => return None,
            OptionValue::Bool(value) => value.to_string(),
            OptionValue::Int(value) => value.to_string(),
            OptionValue::String(value) if self.prints_upper || self.ty == OptionType::TriState => value.to_ascii_uppercase(),
            OptionValue::String(value) => value.clone(),
            OptionValue::Label(label) => label.to_string(),
            OptionValue::Strings(values) => list(values.clone()),
            OptionValue::Labels(labels) => list(labels.iter().map(Label::to_string).collect()),
            OptionValue::Assignments(pairs) => list(pairs.iter().map(|(name, value)| format!("{name}={value}")).collect()),
        })
    }

    /// Whether the option's value is a list.
    pub fn is_list(&self) -> bool {
        matches!(self.ty, OptionType::Strings | OptionType::CommaStrings | OptionType::Labels | OptionType::Assignments)
    }

    /// The value one use of the option, `--name=text`, gives.
    pub fn parse(&self, text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, OptionError> {
        self.convert(text, parse_label)
            .map_err(|problem| OptionError::Invalid(format!("While parsing option --{}={text}: {problem}", self.name)))
    }

    /// The value `text` gives, or the converter's message for why it gives
    /// none, which doesn't say which option it's for.
    pub fn convert(&self, text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, String> {
        let not_a = |kind: &str| format!("'{text}' is not {kind}");
        match self.ty {
            OptionType::Bool => parse_bool(text).map(OptionValue::Bool).ok_or_else(|| not_a("a boolean")),
            OptionType::Int => text.parse().map(OptionValue::Int).map_err(|_| not_a("an int")),
            OptionType::String => self.convert_string(text),
            OptionType::TriState => parse_tri_state(text).map(|value| OptionValue::String(value.to_string())).ok_or_else(|| format!("Not a valid tri-state (auto, yes, no) option value: '{text}' (should be auto or a boolean)")),
            OptionType::Label if text.is_empty() => Ok(OptionValue::Null),
            OptionType::Label => parse_label(text).map(OptionValue::Label),
            OptionType::Strings => Ok(OptionValue::Strings(vec![text.to_string()])),
            OptionType::CommaStrings => Ok(OptionValue::Strings(comma_separated(text).map(str::to_string).collect())),
            OptionType::Labels => parse_labels(text, parse_label),
            OptionType::Assignments => parse_assignment(text).map(|pair| OptionValue::Assignments(vec![pair])),
        }
    }

    /// A string, which must be one of those allowed, if any are; they're
    /// matched ignoring case, as Bazel's enums are.
    fn convert_string(&self, text: &str) -> Result<OptionValue, String> {
        if self.allowed.is_empty() {
            return Ok(OptionValue::String(text.to_string()));
        }
        match self.allowed.iter().find(|allowed| allowed.eq_ignore_ascii_case(text)) {
            Some(allowed) => Ok(OptionValue::String(allowed.to_string())),
            None => Err(format!("Not a valid {}: '{text}' (should be {})", self.allowed_name, english_list(self.allowed))),
        }
    }

    /// The option's value once `next` is given after `previous`: the two
    /// joined if the option takes many values, or else `next`.
    fn combine(&self, previous: &OptionValue, next: OptionValue) -> OptionValue {
        if !self.multiple {
            return next;
        }
        match (previous, next) {
            (OptionValue::Strings(previous), OptionValue::Strings(next)) => OptionValue::Strings([previous.clone(), next].concat()),
            (OptionValue::Labels(previous), OptionValue::Labels(next)) => OptionValue::Labels([previous.clone(), next].concat()),
            (OptionValue::Assignments(previous), OptionValue::Assignments(next)) => OptionValue::Assignments(normalized(previous, next)),
            (_, next) => next,
        }
    }
}

/// `name=value` pairs with `next` after `previous`, as Bazel normalizes
/// them: the last value of each name, sorted by name.
fn normalized(previous: &[(String, String)], next: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut pairs = previous.to_vec();
    for (name, value) in next {
        pairs.retain(|(existing, _)| *existing != name);
        pairs.push((name, value));
    }
    pairs.sort();
    pairs
}

/// A boolean as Bazel's converter reads one, ignoring case.
fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "t" | "y" => Some(true),
        "false" | "0" | "no" | "f" | "n" => Some(false),
        _ => None,
    }
}

fn parse_tri_state(text: &str) -> Option<&'static str> {
    if text.eq_ignore_ascii_case("auto") {
        return Some("auto");
    }
    parse_bool(text).map(|value| if value { "yes" } else { "no" })
}

fn comma_separated(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').filter(|part| !part.is_empty())
}

fn parse_labels(text: &str, parse_label: LabelParser<'_>) -> Result<OptionValue, String> {
    let mut labels = Vec::new();
    for part in comma_separated(text) {
        labels.push(parse_label(part)?);
    }
    Ok(OptionValue::Labels(labels))
}

fn parse_assignment(text: &str) -> Result<(String, String), String> {
    let Some((name, value)) = text.split_once('=') else {
        return Err("Variable definitions must be in the form of a 'name=value' assignment".to_string());
    };
    Ok((name.to_string(), value.to_string()))
}

/// Words joined as Bazel lists them: `a, b or c`.
fn english_list(words: &[&str]) -> String {
    match words.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The value of a Starlark build setting, of the type its rule declares.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum FlagValue {
    Bool(bool),
    Int(i64),
    String(String),
    /// A `string_list` or `string_set`, or a string flag each use adds to.
    Strings(Vec<String>),
    Label(Label),
}

impl FlagValue {
    /// What Bazel calls the value's type, in errors.
    pub fn type_name(&self) -> &'static str {
        match self {
            FlagValue::Bool(_) => "boolean",
            FlagValue::Int(_) => "int",
            FlagValue::String(_) => "string",
            FlagValue::Strings(_) => "list(string)",
            FlagValue::Label(_) => "label",
        }
    }

    /// The value as Bazel prints it when it hashes the settings a
    /// transition changed into an output directory's name. Lists are those
    /// transitions make, which print as Starlark lists.
    pub fn java_text(&self) -> String {
        match self {
            FlagValue::Bool(value) => value.to_string(),
            FlagValue::Int(value) => value.to_string(),
            FlagValue::String(value) => value.clone(),
            FlagValue::Strings(values) => format!("[{}]", values.iter().map(|value| starlark_repr(value)).collect::<Vec<_>>().join(", ")),
            FlagValue::Label(label) => label.to_string(),
        }
    }

    /// The value `text` gives a setting of `like`'s type, as the command
    /// line gives it: lists separated by commas. A setting each use adds to
    /// takes one string.
    pub fn parse(text: &str, like: &FlagValue, multiple: bool, parse_label: LabelParser<'_>) -> Result<FlagValue, String> {
        match like {
            FlagValue::Bool(_) => parse_bool(text).map(FlagValue::Bool).ok_or_else(|| "not a boolean".to_string()),
            FlagValue::Int(_) => text.parse().map(FlagValue::Int).map_err(|_| "not an int".to_string()),
            FlagValue::String(_) => Ok(FlagValue::String(text.to_string())),
            FlagValue::Strings(_) if multiple => Ok(FlagValue::Strings(vec![text.to_string()])),
            FlagValue::Strings(_) => Ok(FlagValue::Strings(comma_separated(text).map(str::to_string).collect())),
            FlagValue::Label(_) => parse_label(text).map(FlagValue::Label),
        }
    }
}

/// A string as Starlark's `repr()` writes it.
fn starlark_repr(text: &str) -> String {
    let mut repr = String::with_capacity(text.len() + 2);
    repr.push('"');
    for c in text.chars() {
        match c {
            '"' => repr.push_str("\\\""),
            '\\' => repr.push_str("\\\\"),
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            c => repr.push(c),
        }
    }
    repr.push('"');
    repr
}

/// The options that decide how targets are built.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BuildOptions {
    /// The native options that differ from their defaults, sorted by name.
    native: Vec<(String, OptionValue)>,
    /// Starlark build settings that differ from their defaults, by the
    /// label of the setting, not an alias of it, sorted by label.
    pub starlark_flags: Vec<(Label, FlagValue)>,
    /// For options a transition made: the top-level configuration they
    /// came from, which their output directory is named against.
    pub baseline: Option<crate::ConfigId>,
    /// The environment of actions that use the default shell environment,
    /// resolved against the client's, sorted by name.
    pub action_env: Vec<(String, String)>,
}

impl BuildOptions {
    /// A native option's value.
    pub fn get(&self, name: &str) -> Result<&OptionValue, OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let set = self.native.binary_search_by(|(candidate, _)| candidate.as_str().cmp(name)).ok();
        Ok(match set {
            Some(index) => &self.native[index].1,
            None => default_of(option),
        })
    }

    /// Set a native option, which must be one bb knows, to a value of its
    /// type.
    pub fn set(&mut self, name: &str, value: OptionValue) -> Result<(), OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let index = self.native.binary_search_by(|(candidate, _)| candidate.as_str().cmp(name));
        match (index, value == *default_of(option)) {
            (Ok(index), true) => {
                self.native.remove(index);
            }
            (Ok(index), false) => self.native[index].1 = value,
            (Err(_), true) => {}
            (Err(index), false) => self.native.insert(index, (name.to_string(), value)),
        }
        Ok(())
    }

    /// Give a native option on the command line, `--name=text`, after the
    /// uses of it before.
    pub fn apply(&mut self, name: &str, text: &str, parse_label: LabelParser<'_>) -> Result<(), OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let next = option.parse(text, parse_label)?;
        let value = option.combine(self.get(name)?, next);
        self.set(name, value)
    }

    /// Whether a native option has the value `expected`, as
    /// `config_setting(values = {name: expected})` asks: equal to it, or
    /// for lists, containing all of it. For `name=value` pairs, the last
    /// with its name must have its value.
    pub fn matches(&self, name: &str, expected: &str, parse_label: LabelParser<'_>) -> Result<bool, OptionError> {
        let option = native_option(name).ok_or_else(|| OptionError::Unknown(name.to_string()))?;
        let expected = option.parse(expected, parse_label)?;
        let actual = self.get(name)?;
        Ok(match (actual, &expected) {
            (OptionValue::Strings(actual), OptionValue::Strings(expected)) => list_contains(actual, expected),
            (OptionValue::Labels(actual), OptionValue::Labels(expected)) => list_contains(actual, expected),
            (OptionValue::Assignments(actual), OptionValue::Assignments(expected)) => assignment_matches(actual, expected),
            (actual, expected) => actual == expected,
        })
    }

    /// A string option's value, or `""` if it has none.
    pub fn string(&self, name: &str) -> &str {
        match self.get(name) {
            Ok(OptionValue::String(value)) => value,
            _ => "",
        }
    }

    /// A boolean option's value.
    pub fn bool(&self, name: &str) -> bool {
        matches!(self.get(name), Ok(OptionValue::Bool(true)))
    }

    /// A list of strings' values.
    pub fn strings(&self, name: &str) -> &[String] {
        match self.get(name) {
            Ok(OptionValue::Strings(values)) => values,
            _ => &[],
        }
    }

    /// A label option's value.
    pub fn label(&self, name: &str) -> Option<Label> {
        match self.get(name) {
            Ok(OptionValue::Label(label)) => Some(*label),
            _ => None,
        }
    }

    /// A list of labels' values.
    pub fn labels(&self, name: &str) -> &[Label] {
        match self.get(name) {
            Ok(OptionValue::Labels(labels)) => labels,
            _ => &[],
        }
    }

    /// The value of the last `--define` of `name`.
    pub fn define(&self, name: &str) -> Option<&str> {
        let Ok(OptionValue::Assignments(defines)) = self.get("define") else {
            return None;
        };
        defines.iter().rev().find(|(define, _)| define == name).map(|(_, value)| value.as_str())
    }

    /// The native options that differ from their defaults.
    pub fn changed(&self) -> &[(String, OptionValue)] {
        &self.native
    }

    /// A Starlark build setting's value, if it differs from its default.
    pub fn flag(&self, label: Label) -> Option<&FlagValue> {
        self.starlark_flags.iter().find(|(flag, _)| *flag == label).map(|(_, value)| value)
    }

    /// Set a Starlark build setting, leaving it out when it's `default`, as
    /// Bazel does.
    pub fn set_flag(&mut self, label: Label, value: FlagValue, default: &FlagValue) {
        self.starlark_flags.retain(|(flag, _)| *flag != label);
        if value != *default {
            self.starlark_flags.push((label, value));
            self.starlark_flags.sort_by(|(a, _), (b, _)| a.cmp(b));
        }
    }
}

fn list_contains<T: PartialEq>(actual: &[T], expected: &[T]) -> bool {
    if actual.is_empty() || expected.is_empty() {
        return actual.is_empty() && expected.is_empty();
    }
    expected.iter().all(|value| actual.contains(value))
}

fn assignment_matches(actual: &[(String, String)], expected: &[(String, String)]) -> bool {
    let [(name, value)] = expected else {
        return false;
    };
    actual.iter().rev().find(|(candidate, _)| candidate == name).is_some_and(|(_, actual)| actual == value)
}
