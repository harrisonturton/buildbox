//! `ctx.fragments` and `ctx.configuration`: parts of the configuration
//! rules read.

use allocative::Allocative;
use bazel_config::{BuildOptions, OptionValue};
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::opaque::Opaque;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// `ctx.fragments`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct Fragments(
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    #[allocative(skip)]
    pub BuildOptions,
);

starlark_simple_value!(Fragments);

impl fmt::Display for Fragments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<fragments>")
    }
}

#[starlark_value(type = "fragments")]
impl<'v> StarlarkValue<'v> for Fragments {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(Fragment {
            name: attribute.to_string(),
            options: self.0.clone(),
        }))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

/// A configuration fragment, such as `cpp`. Members bb doesn't know are
/// opaque.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct Fragment {
    pub name: String,
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    #[allocative(skip)]
    pub options: BuildOptions,
}

starlark_simple_value!(Fragment);

impl fmt::Display for Fragment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} fragment>", self.name)
    }
}

#[starlark_value(type = "fragment")]
impl<'v> StarlarkValue<'v> for Fragment {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("fragment", fragment_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let words = |option: &str| heap.alloc(AllocList(self.options.strings(option)));
        let value = match (self.name.as_str(), attribute) {
            ("cpp", "copts") => words("copt"),
            ("cpp", "cxxopts") => words("cxxopt"),
            ("cpp", "conlyopts") => words("conlyopt"),
            ("cpp", "linkopts") => words("linkopt"),
            ("cpp", "objccopts" | "lto_backend_options") => heap.alloc(AllocList::EMPTY),
            ("cpp", "custom_malloc" | "_fdo_prefetch_hints_label") => Value::new_none(),
            ("cpp", "apple_generate_dsym" | "objc_generate_linkmap" | "objc_should_strip_binary") => Value::new_bool(false),
            ("cpp", "_dont_enable_host_nonhost") => Value::new_bool(true),
            ("proto", "experimental_protoc_opts") => words("protocopt"),
            ("proto", "cc_proto_library_header_suffixes") => heap.alloc(AllocList([".pb.h"])),
            ("proto", "cc_proto_library_source_suffixes") => heap.alloc(AllocList([".pb.cc"])),
            ("apple", "prefer_mutual_xcode") => Value::new_bool(true),
            ("apple", "include_xcode_exec_requirements") => Value::new_bool(false),
            ("apple", flag) if flag.ends_with("_flag") => Value::new_none(),
            ("java", field) => match java_field(&self.options, field, heap) {
                Some(value) => value,
                None => heap.alloc(Opaque::new(&format!("ctx.fragments.java.{attribute}"))),
            },
            ("py", "build_python_zip") => Value::new_bool(self.options.string("build_python_zip") == "yes"),
            ("py", "default_to_explicit_init_py") => Value::new_bool(self.options.bool("incompatible_default_to_explicit_init_py")),
            ("py", "disallow_native_rules") => Value::new_bool(self.options.bool("incompatible_python_disallow_native_rules")),
            ("py", "use_toolchains") => Value::new_bool(true),
            ("py", "default_python_version") => heap.alloc("PY3"),
            ("bazel_py", "python_path") => heap.alloc(self.options.get("python_path").ok().and_then(string_value).unwrap_or("python")),
            ("bazel_py", "python_import_all_repositories") => Value::new_bool(self.options.bool("experimental_python_import_all_repositories")),
            ("bazel_py", "python_top") => Value::new_none(),
            _ => heap.alloc(Opaque::new(&format!("ctx.fragments.{}.{attribute}", self.name))),
        };
        Some(value)
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

/// A field of the `java` fragment, as Bazel's `JavaConfiguration` gives it,
/// or `None` for one bb doesn't know.
fn java_field<'v>(options: &BuildOptions, field: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    let value = match field {
        "default_javac_flags" => heap.alloc(AllocList(javac_flags(options).iter().map(String::as_str))),
        "default_javac_flags_depset" => {
            let joined = joined_javac_flags(options);
            let direct = joined.iter().map(|flags| heap.alloc(flags.as_str())).collect();
            heap.alloc(Depset {
                direct,
                transitive: Vec::new(),
                order: Order::Default,
            })
        }
        "strict_java_deps" => heap.alloc(options.string("strict_java_deps").to_ascii_lowercase()),
        "default_jvm_opts" => heap.alloc(AllocList(options.strings("jvmopt"))),
        "one_version_enforcement_level" => heap.alloc(options.string("experimental_one_version_enforcement").to_ascii_uppercase()),
        "one_version_enforcement_on_java_tests" => Value::new_bool(options.bool("one_version_enforcement_on_java_tests")),
        "add_test_support_to_compile_deps" => Value::new_bool(options.bool("experimental_add_test_support_to_compile_time_deps")),
        "run_android_lint" => Value::new_bool(options.bool("experimental_run_android_lint_on_java_rules")),
        "multi_release_deploy_jars" => Value::new_bool(options.bool("incompatible_multi_release_deploy_jars")),
        "plugins" => heap.alloc(AllocList(options.labels("plugin").iter().map(|label| LabelValue(*label)))),
        "bytecode_optimizer_mnemonic" => {
            let optimizers = options.string("bytecode_optimizers");
            heap.alloc(optimizers.split_once('=').map_or(optimizers, |(name, _)| name))
        }
        "split_bytecode_optimization_pass" => Value::new_bool(options.bool("split_bytecode_optimization_pass")),
        "bytecode_optimization_pass_actions" => match options.get("bytecode_optimization_pass_actions") {
            Ok(OptionValue::Int(actions)) => heap.alloc(*actions as i32),
            _ => return None,
        },
        "enforce_proguard_file_extension" => Value::new_bool(options.bool("enforce_proguard_file_extension")),
        _ => return None,
    };
    Some(value)
}

/// `--javacopt`s as Bazel keeps them: tokenized, then quoted and joined
/// into one, or none if there are none. Options that don't tokenize are
/// kept as they are.
fn joined_javac_flags(options: &BuildOptions) -> Option<String> {
    let mut tokens = Vec::new();
    for option in options.strings("javacopt") {
        match crate::shell::tokenize(option) {
            Ok(words) => tokens.extend(words),
            Err(_) => tokens.push(option.clone()),
        }
    }
    match tokens.is_empty() {
        true => None,
        false => Some(tokens.iter().map(|token| crate::shell::escape(token)).collect::<Vec<_>>().join(" ")),
    }
}

/// `--javacopt`s as the `java` fragment lists them: [`joined_javac_flags`]
/// tokenized again.
fn javac_flags(options: &BuildOptions) -> Vec<String> {
    let Some(joined) = joined_javac_flags(options) else {
        return Vec::new();
    };
    crate::shell::tokenize(&joined).unwrap_or_else(|_| vec![joined])
}

fn string_value(value: &OptionValue) -> Option<&str> {
    match value {
        OptionValue::String(text) => Some(text),
        _ => None,
    }
}

/// The options of a fragment, which must be `name`.
fn fragment_options<'v>(this: Value<'v>, name: &str) -> anyhow::Result<&'v BuildOptions> {
    let fragment = this.downcast_ref::<Fragment>().ok_or_else(|| anyhow::anyhow!("expected a fragment"))?;
    if fragment.name != name {
        anyhow::bail!("the {} fragment has no such method", fragment.name);
    }
    Ok(&fragment.options)
}

/// Fragment methods that answer as Bazel's do with default options.
#[starlark_module]
fn fragment_methods(builder: &mut MethodsBuilder) {
    /// `java.use_header_compilation()`: `--java_header_compilation`.
    fn use_header_compilation<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fragment_options(this, "java")?.bool("java_header_compilation"))
    }

    /// `java.generate_java_deps()`: whether compilations write their
    /// dependencies, as `--java_deps` or a reduced classpath needs.
    fn generate_java_deps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let options = fragment_options(this, "java")?;
        Ok(options.bool("java_deps") || options.string("experimental_java_classpath") != "off")
    }

    /// `java.reduce_java_classpath()`: `--experimental_java_classpath`, as
    /// Bazel's enum names it.
    fn reduce_java_classpath<'v>(this: Value<'v>) -> anyhow::Result<String> {
        Ok(fragment_options(this, "java")?.string("experimental_java_classpath").to_ascii_uppercase())
    }

    fn enforce_explicit_java_test_deps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fragment_options(this, "java")?.bool("explicit_java_test_deps"))
    }

    fn use_ijars<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fragment_options(this, "java")?.bool("use_ijars"))
    }

    /// Always true in Bazel 9, which is removing the method.
    fn use_header_compilation_direct_deps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        fragment_options(this, "java")?;
        Ok(true)
    }

    fn disallow_java_import_exports<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fragment_options(this, "java")?.bool("incompatible_disallow_java_import_exports"))
    }

    fn auto_create_java_test_deploy_jars<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        Ok(fragment_options(this, "java")?.bool("experimental_java_test_auto_create_deploy_jar"))
    }

    /// `cpp.compilation_mode()`: `fastbuild`, `dbg` or `opt`.
    fn compilation_mode<'v>(this: Value<'v>) -> anyhow::Result<String> {
        let fragment = this.downcast_ref::<Fragment>().ok_or_else(|| anyhow::anyhow!("expected a fragment"))?;
        Ok(fragment.options.string("compilation_mode").to_string())
    }

    /// `cpp.dynamic_mode()`: `--dynamic_mode`, as Bazel's enum names it.
    fn dynamic_mode<'v>(this: Value<'v>) -> anyhow::Result<&'static str> {
        let _ = this;
        Ok("DEFAULT")
    }

    fn lto_index_options<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    fn strip_opts<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    fn extra_allowlisted_feature_layering_check_macros<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    fn force_layering_check_features<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(AllocList::EMPTY))
    }

    /// Options on by default: `--start_end_lib`, `--legacy_whole_archive`,
    /// `--incompatible_remove_legacy_whole_archive`,
    /// `--interface_shared_objects`, `--share_native_deps`,
    /// `--experimental_link_static_libraries_once`, `--proto_profile` and
    /// `--cc_dotd_files`.
    fn start_end_lib<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn legacy_whole_archive<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn incompatible_remove_legacy_whole_archive<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn interface_shared_objects<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn share_native_deps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn experimental_link_static_libraries_once<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn proto_profile<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn should_generate_dotd_files<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn objc_should_generate_dotd_files<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    /// Always true in Bazel 9.
    fn incompatible_use_specific_tool_files<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    fn disable_nocopts<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }

    /// Options off by default.
    fn use_llvm_coverage_map_format<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn include_scanning<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn objc_enable_binary_stripping<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn experimental_cc_implementation_deps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn experimental_cpp_modules<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn process_headers_in_dependencies<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn save_feature_state<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn save_temps<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn build_test_dwp<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// Unset paths and labels: FDO profiles, Propeller profiles and
    /// `--grte_top`.
    fn fdo_path<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn cs_fdo_path<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn fdo_instrument<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn cs_fdo_instrument<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn propeller_optimize_absolute_cc_profile<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn propeller_optimize_absolute_ld_profile<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn grte_top<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }
    fn fission_active_for_current_compilation_mode<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn force_pic<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `cpp.minimum_os_version()`: `--minimum_os_version`, unset.
    fn minimum_os_version<'v>(this: Value<'v>) -> anyhow::Result<starlark::values::none::NoneType> {
        let _ = this;
        Ok(starlark::values::none::NoneType)
    }

    fn generate_llvm_lcov<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// Whether `--strip` says to strip: always, or sometimes, which is in
    /// fastbuild builds.
    fn should_strip_binaries<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let fragment = this.downcast_ref::<Fragment>().ok_or_else(|| anyhow::anyhow!("expected a fragment"))?;
        Ok(match fragment.options.string("strip") {
            "always" => true,
            "sometimes" => fragment.options.string("compilation_mode") == "fastbuild",
            _ => false,
        })
    }
}

/// `ctx.configuration`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct Configuration(
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    #[allocative(skip)]
    pub BuildOptions,
);

starlark_simple_value!(Configuration);

impl fmt::Display for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<configuration>")
    }
}

#[starlark_value(type = "configuration")]
impl<'v> StarlarkValue<'v> for Configuration {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("configuration", configuration_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let value = match attribute {
            "coverage_enabled" => Value::new_bool(false),
            "host_path_separator" => heap.alloc(":"),
            "default_shell_env" => heap.alloc(AllocDict(self.0.action_env.iter().map(|(name, value)| (name.as_str(), value.as_str())))),
            "test_env" => heap.alloc(AllocDict(Vec::<(&str, &str)>::new())),
            _ => return None,
        };
        Some(value)
    }

    fn dir_attr(&self) -> Vec<String> {
        ["coverage_enabled", "host_path_separator", "default_shell_env", "test_env"].iter().map(|name| name.to_string()).collect()
    }
}

#[starlark_module]
fn configuration_methods(builder: &mut MethodsBuilder) {
    /// Whether external repositories' outputs sit beside the main
    /// repository's, as `--experimental_sibling_repository_layout` asks;
    /// bb lays them out as Bazel does by default.
    fn is_sibling_repository_layout<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// Whether generated files have a directory of their own, apart from
    /// `bin`: not since Bazel merged them.
    fn has_separate_genfiles_directory<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    fn stamp_binaries<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let configuration = this.downcast_ref::<Configuration>().ok_or_else(|| anyhow::anyhow!("expected a configuration"))?;
        Ok(configuration.0.bool("stamp"))
    }

    fn is_tool_configuration<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let configuration = this.downcast_ref::<Configuration>().ok_or_else(|| anyhow::anyhow!("expected a configuration"))?;
        Ok(configuration.0.bool("is exec configuration"))
    }

    fn runfiles_enabled<'v>(this: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(true)
    }
}
