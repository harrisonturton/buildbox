//! `cc_common`, `CcInfo`, and the attributes and methods of
//! `CcToolchainInfo`: the C++ API rules use.

use super::config::{CcToolchainConfig, Reader};
use super::features::{FeatureConfiguration, ToolchainFeatures, Variable, Variables};
use super::values::{CcStruct, CcToolchain, FeatureConfigurationValue, NativeProvider, ToolchainConfigValue, VariablesValue};
use crate::file_value::FileValue;
use crate::state::RuleState;
use allocative::Allocative;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::opaque::Opaque;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::tuple::AllocTuple;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// The attributes of `CcToolchainInfo` besides its files.
pub const TOOLCHAIN_ATTRS: &[&str] = &[
    "compiler",
    "compiler_executable",
    "cpu",
    "target_gnu_system_name",
    "sysroot",
    "libc",
    "ar_executable",
    "ld_executable",
    "nm_executable",
    "objcopy_executable",
    "objdump_executable",
    "strip_executable",
    "preprocessor_executable",
    "gcov_executable",
    "built_in_include_directories",
    "toolchain_id",
];

/// An attribute of a toolchain from its configuration.
pub fn toolchain_attr<'v>(config: &CcToolchainConfig, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    let tool = |name: &str| heap.alloc(config.tool_path(name).unwrap_or_default());
    let value = match attribute {
        "compiler" => heap.alloc(config.compiler.as_str()),
        "compiler_executable" => tool("gcc"),
        "cpu" => heap.alloc(config.target_cpu.as_str()),
        "target_gnu_system_name" => heap.alloc(config.target_system_name.as_str()),
        "sysroot" => config.builtin_sysroot.as_deref().map(|sysroot| heap.alloc(sysroot)).unwrap_or_else(Value::new_none),
        "libc" => heap.alloc(config.target_libc.as_str()),
        "ar_executable" => tool("ar"),
        "ld_executable" => tool("ld"),
        "nm_executable" => tool("nm"),
        "objcopy_executable" => tool("objcopy"),
        "objdump_executable" => tool("objdump"),
        "strip_executable" => tool("strip"),
        "preprocessor_executable" => tool("cpp"),
        "gcov_executable" => tool("gcov"),
        "built_in_include_directories" => heap.alloc(AllocList(config.cxx_builtin_include_directories.iter().map(String::as_str))),
        "toolchain_id" => heap.alloc(config.toolchain_identifier.as_str()),
        _ => return None,
    };
    Some(value)
}

fn unpack_features<'v>(value: Value<'v>) -> anyhow::Result<&'v Arc<FeatureConfiguration>> {
    value
        .downcast_ref::<FeatureConfigurationValue>()
        .map(|configuration| &configuration.0)
        .ok_or_else(|| anyhow::anyhow!("expected a FeatureConfiguration, not {}", value.get_type()))
}

fn variables<'v>(value: Option<Value<'v>>) -> anyhow::Result<Arc<Variables>> {
    match value.filter(|value| !value.is_none()) {
        None => Ok(Arc::new(Variables::default())),
        Some(value) => value
            .downcast_ref::<VariablesValue>()
            .map(|variables| variables.0.clone())
            .ok_or_else(|| anyhow::anyhow!("expected build variables, not {}", value.get_type())),
    }
}

fn toolchain<'v>(value: Value<'v>) -> anyhow::Result<&'v CcToolchain<'v>> {
    CcToolchain::from_value(value).ok_or_else(|| anyhow::anyhow!("expected a CcToolchainInfo, not {}", value.get_type()))
}

#[starlark_module]
pub fn toolchain_methods(builder: &mut MethodsBuilder) {
    fn needs_pic_for_dynamic_libraries<'v>(this: Value<'v>, feature_configuration: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        Ok(unpack_features(feature_configuration)?.is_enabled("supports_pic"))
    }

    fn static_runtime_lib<'v>(this: Value<'v>, feature_configuration: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = feature_configuration;
        Ok(toolchain(this)?.file_set("static_runtime_lib").unwrap_or_else(|| empty_depset(heap)))
    }

    fn dynamic_runtime_lib<'v>(this: Value<'v>, feature_configuration: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = feature_configuration;
        Ok(toolchain(this)?.file_set("dynamic_runtime_lib").unwrap_or_else(|| empty_depset(heap)))
    }
}

pub fn empty_depset(heap: Heap<'_>) -> Value<'_> {
    depset(Vec::new(), Vec::new(), Order::Default, heap)
}

pub fn depset<'v>(direct: Vec<Value<'v>>, transitive: Vec<Value<'v>>, order: Order, heap: Heap<'v>) -> Value<'v> {
    heap.alloc(Depset { direct, transitive, order })
}

/// The items of a list, tuple or depset; nothing for `None`.
pub fn items<'v>(value: Option<Value<'v>>) -> Vec<Value<'v>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Vec::new();
    };
    if let Some(depset) = Depset::from_value(value) {
        return depset.to_list();
    }
    bazel_starlark::convert::items(value).unwrap_or_default()
}

fn strings(value: Option<Value<'_>>) -> Vec<String> {
    items(value).into_iter().map(crate::args::stringify).collect()
}

/// A depset of a value's items, or the value itself if it's a depset.
fn as_depset<'v>(value: Option<Value<'v>>, heap: Heap<'v>) -> Value<'v> {
    match value.filter(|value| !value.is_none()) {
        Some(value) if Depset::from_value(value).is_some() => value,
        Some(value) => depset(bazel_starlark::convert::items(value).unwrap_or_default(), Vec::new(), Order::Default, heap),
        None => empty_depset(heap),
    }
}

/// A `CompilationContext`.
pub fn compilation_context<'v>(fields: CompilationFields<'v>, heap: Heap<'v>) -> Value<'v> {
    let direct_headers = heap.alloc(AllocList(fields.direct_public_headers.iter().chain(&fields.direct_private_headers).copied()));
    heap.alloc(CcStruct::new(
        "CompilationContext",
        vec![
            ("headers", fields.headers),
            ("direct_headers", direct_headers),
            ("direct_public_headers", heap.alloc(AllocList(fields.direct_public_headers))),
            ("direct_private_headers", heap.alloc(AllocList(fields.direct_private_headers))),
            ("direct_textual_headers", heap.alloc(AllocList::EMPTY)),
            ("includes", fields.includes),
            ("quote_includes", fields.quote_includes),
            ("system_includes", fields.system_includes),
            ("framework_includes", fields.framework_includes),
            ("external_includes", empty_depset(heap)),
            ("defines", fields.defines),
            ("local_defines", fields.local_defines),
            ("validation_artifacts", empty_depset(heap)),
        ],
    ))
}

/// The parts of a `CompilationContext`, as depsets except the direct
/// headers.
pub struct CompilationFields<'v> {
    pub headers: Value<'v>,
    pub direct_public_headers: Vec<Value<'v>>,
    pub direct_private_headers: Vec<Value<'v>>,
    pub includes: Value<'v>,
    pub quote_includes: Value<'v>,
    pub system_includes: Value<'v>,
    pub framework_includes: Value<'v>,
    pub defines: Value<'v>,
    pub local_defines: Value<'v>,
}

impl<'v> CompilationFields<'v> {
    pub fn empty(heap: Heap<'v>) -> CompilationFields<'v> {
        CompilationFields {
            headers: empty_depset(heap),
            direct_public_headers: Vec::new(),
            direct_private_headers: Vec::new(),
            includes: empty_depset(heap),
            quote_includes: empty_depset(heap),
            system_includes: empty_depset(heap),
            framework_includes: empty_depset(heap),
            defines: empty_depset(heap),
            local_defines: empty_depset(heap),
        }
    }
}

/// Merge compilation contexts: their depsets, transitively.
pub fn merge_compilation_contexts<'v>(contexts: &[Value<'v>], heap: Heap<'v>) -> Value<'v> {
    let field = |name: &str| -> Value<'v> {
        let transitive: Vec<Value<'v>> = contexts.iter().filter_map(|context| CcStruct::field_of(*context, "CompilationContext", name)).collect();
        depset(Vec::new(), transitive, Order::Default, heap)
    };
    let mut fields = CompilationFields::empty(heap);
    fields.headers = field("headers");
    fields.includes = field("includes");
    fields.quote_includes = field("quote_includes");
    fields.system_includes = field("system_includes");
    fields.framework_includes = field("framework_includes");
    fields.defines = field("defines");
    compilation_context(fields, heap)
}

/// A `LinkingContext` of linker inputs, in topological order so that a
/// library comes before those it depends on.
pub fn linking_context<'v>(direct: Vec<Value<'v>>, transitive: Vec<Value<'v>>, heap: Heap<'v>) -> Value<'v> {
    let inputs = depset(direct, transitive, Order::Topological, heap);
    heap.alloc(CcStruct::new("LinkingContext", vec![("linker_inputs", inputs)]))
}

fn linker_inputs_of<'v>(context: Value<'v>) -> Option<Value<'v>> {
    CcStruct::field_of(context, "LinkingContext", "linker_inputs")
}

/// Merge linking contexts.
pub fn merge_linking_contexts<'v>(contexts: &[Value<'v>], heap: Heap<'v>) -> Value<'v> {
    let transitive = contexts.iter().filter_map(|context| linker_inputs_of(*context)).collect();
    linking_context(Vec::new(), transitive, heap)
}

/// A `CcInfo`.
pub fn cc_info<'v>(compilation: Value<'v>, linking: Value<'v>, heap: Heap<'v>) -> Value<'v> {
    let debug = heap.alloc(CcStruct::new("DebugContext", vec![("files", empty_depset(heap)), ("pic_files", empty_depset(heap))]));
    heap.alloc(CcStruct::new(
        "CcInfo",
        vec![("compilation_context", compilation), ("linking_context", linking), ("debug_context", debug)],
    ))
}

/// Merge `CcInfo`s: their compilation contexts, and linking contexts.
pub fn merge_cc_infos<'v>(infos: &[Value<'v>], heap: Heap<'v>) -> Value<'v> {
    let compilation: Vec<Value<'v>> = infos.iter().filter_map(|info| CcStruct::field_of(*info, "CcInfo", "compilation_context")).collect();
    let linking: Vec<Value<'v>> = infos.iter().filter_map(|info| CcStruct::field_of(*info, "CcInfo", "linking_context")).collect();
    cc_info(merge_compilation_contexts(&compilation, heap), merge_linking_contexts(&linking, heap), heap)
}

/// A `LinkerInput`.
pub fn linker_input<'v>(owner: Value<'v>, libraries: Vec<Value<'v>>, user_link_flags: Vec<String>, additional_inputs: Vec<Value<'v>>, heap: Heap<'v>) -> Value<'v> {
    heap.alloc(CcStruct::new(
        "LinkerInput",
        vec![
            ("owner", owner),
            ("libraries", heap.alloc(AllocList(libraries))),
            ("user_link_flags", heap.alloc(AllocList(user_link_flags))),
            ("additional_inputs", heap.alloc(AllocList(additional_inputs))),
            ("linkstamps", heap.alloc(AllocList::EMPTY)),
        ],
    ))
}

/// The files of a `LibraryToLink`.
#[derive(Default)]
pub struct LibraryFiles<'v> {
    pub static_library: Option<Value<'v>>,
    pub pic_static_library: Option<Value<'v>>,
    pub dynamic_library: Option<Value<'v>>,
    pub interface_library: Option<Value<'v>>,
    pub objects: Vec<Value<'v>>,
    pub pic_objects: Vec<Value<'v>>,
    pub alwayslink: bool,
}

/// A `LibraryToLink`.
pub fn library_to_link<'v>(files: LibraryFiles<'v>, heap: Heap<'v>) -> Value<'v> {
    let or_none = |value: Option<Value<'v>>| value.unwrap_or_else(Value::new_none);
    heap.alloc(CcStruct::new(
        "LibraryToLink",
        vec![
            ("static_library", or_none(files.static_library)),
            ("pic_static_library", or_none(files.pic_static_library)),
            ("dynamic_library", or_none(files.dynamic_library)),
            ("interface_library", or_none(files.interface_library)),
            ("resolved_symlink_dynamic_library", or_none(files.dynamic_library)),
            ("resolved_symlink_interface_library", or_none(files.interface_library)),
            ("objects", heap.alloc(AllocList(files.objects))),
            ("pic_objects", heap.alloc(AllocList(files.pic_objects))),
            ("lto_bitcode_files", heap.alloc(AllocList::EMPTY)),
            ("pic_lto_bitcode_files", heap.alloc(AllocList::EMPTY)),
            ("alwayslink", Value::new_bool(files.alwayslink)),
        ],
    ))
}

/// `CcInfo(compilation_context = None, linking_context = None, ...)`.
fn construct_cc_info<'v>(args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    let heap = eval.heap();
    let names = args.names_map().map_err(|error| anyhow::anyhow!("{error}"))?;
    let named = |name: &str| names.iter().find(|(key, _)| key.as_str() == name).map(|(_, value)| *value).filter(|value| !value.is_none());
    let compilation = named("compilation_context").unwrap_or_else(|| compilation_context(CompilationFields::empty(heap), heap));
    let linking = named("linking_context").unwrap_or_else(|| linking_context(Vec::new(), Vec::new(), heap));
    Ok(cc_info(compilation, linking, heap))
}

/// Feature names a configuration requests for a rule besides those it
/// asks for: the compilation mode's, such as `opt`.
fn implicit_features(eval: &Evaluator<'_, '_, '_>) -> Vec<String> {
    match RuleState::get(eval) {
        Ok(state) => vec![state.config.options.compilation_mode.clone()],
        Err(_) => Vec::new(),
    }
}

/// `cc_common`: the C++ API, with members bb doesn't implement opaque.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct CcCommon;

starlark_simple_value!(CcCommon);

impl fmt::Display for CcCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<cc_common>")
    }
}

/// The providers `cc_common` has as attributes.
const CC_COMMON_PROVIDERS: &[&str] = &["CcToolchainInfo"];

#[starlark_value(type = "cc_common")]
impl<'v> StarlarkValue<'v> for CcCommon {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("cc_common", cc_common_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if CC_COMMON_PROVIDERS.contains(&attribute) {
            return crate::providers::builtin(attribute).ok();
        }
        if attribute == "do_not_use_tools_cpp_compiler_present" {
            return Some(Value::new_bool(true));
        }
        Some(heap.alloc(Opaque::new(&format!("cc_common.{attribute}"))))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

#[starlark_module]
fn cc_common_methods(builder: &mut MethodsBuilder) {
    fn is_cc_toolchain_resolution_enabled_do_not_use<'v>(this: Value<'v>, ctx: Value<'v>) -> anyhow::Result<bool> {
        let _ = (this, ctx);
        Ok(true)
    }

    fn configure_features<'v>(
        this: Value<'v>,
        #[starlark(require = named)] ctx: Option<Value<'v>>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] language: Option<Value<'v>>,
        #[starlark(require = named)] requested_features: Option<Value<'v>>,
        #[starlark(require = named)] unsupported_features: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FeatureConfigurationValue> {
        let _ = (this, ctx, language);
        let toolchain = toolchain(cc_toolchain)?;
        let mut requested = strings(requested_features);
        requested.extend(implicit_features(eval));
        let unsupported = strings(unsupported_features);
        let configuration = toolchain.config.features.configure(&requested, &unsupported)?;
        Ok(FeatureConfigurationValue(Arc::new(configuration)))
    }

    fn is_enabled<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] feature_name: &str,
    ) -> anyhow::Result<bool> {
        let _ = this;
        Ok(unpack_features(feature_configuration)?.is_enabled(feature_name))
    }

    fn action_is_enabled<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] action_name: &str,
    ) -> anyhow::Result<bool> {
        let _ = this;
        Ok(unpack_features(feature_configuration)?.action_is_configured(action_name))
    }

    fn get_tool_for_action<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] action_name: &str,
    ) -> anyhow::Result<String> {
        let _ = this;
        Ok(unpack_features(feature_configuration)?.tool(action_name)?.path.clone())
    }

    fn get_execution_requirements<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] action_name: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let configuration = unpack_features(feature_configuration)?;
        let requirements = configuration.tool(action_name).map(|tool| tool.execution_requirements.clone()).unwrap_or_default();
        Ok(heap.alloc(AllocList(requirements)))
    }

    fn get_memory_inefficient_command_line<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] action_name: &str,
        #[starlark(require = named)] variables: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let configuration = unpack_features(feature_configuration)?;
        let command = configuration.command_line(action_name, &*self::variables(variables)?)?;
        Ok(heap.alloc(AllocList(command)))
    }

    fn get_environment_variables<'v>(
        this: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] action_name: &str,
        #[starlark(require = named)] variables: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let configuration = unpack_features(feature_configuration)?;
        let env = configuration.environment(action_name, &*self::variables(variables)?)?;
        Ok(heap.alloc(AllocDict(env)))
    }

    fn empty_variables<'v>(this: Value<'v>) -> anyhow::Result<VariablesValue> {
        let _ = this;
        Ok(VariablesValue(Arc::new(Variables::default())))
    }

    fn create_compile_variables<'v>(
        this: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] source_file: Option<Value<'v>>,
        #[starlark(require = named)] output_file: Option<Value<'v>>,
        #[starlark(require = named)] user_compile_flags: Option<Value<'v>>,
        #[starlark(require = named)] include_directories: Option<Value<'v>>,
        #[starlark(require = named)] quote_include_directories: Option<Value<'v>>,
        #[starlark(require = named)] system_include_directories: Option<Value<'v>>,
        #[starlark(require = named)] framework_include_directories: Option<Value<'v>>,
        #[starlark(require = named)] preprocessor_defines: Option<Value<'v>>,
        #[starlark(require = named)] thinlto_index: Option<Value<'v>>,
        #[starlark(require = named)] thinlto_input_bitcode_file: Option<Value<'v>>,
        #[starlark(require = named)] thinlto_output_object_file: Option<Value<'v>>,
        #[starlark(require = named, default = false)] use_pic: bool,
        #[starlark(require = named, default = false)] add_legacy_cxx_options: bool,
        #[starlark(require = named)] variables_extension: Option<Value<'v>>,
        #[starlark(require = named)] strip_opts: Option<Value<'v>>,
        #[starlark(require = named)] input_file: Option<Value<'v>>,
    ) -> anyhow::Result<VariablesValue> {
        let _ = (this, feature_configuration, thinlto_index, thinlto_input_bitcode_file, thinlto_output_object_file, add_legacy_cxx_options, strip_opts, input_file);
        let toolchain = toolchain(cc_toolchain)?;
        let mut variables = toolchain_variables(&toolchain.config);
        let file = |value: Option<Value<'v>>| value.filter(|value| !value.is_none()).map(crate::args::stringify);
        if let Some(source) = file(source_file) {
            variables.string("source_file", source);
        }
        if let Some(output) = file(output_file) {
            variables.string("output_file", output);
        }
        variables.strings("user_compile_flags", strings(user_compile_flags));
        variables.strings("include_paths", strings(include_directories));
        variables.strings("quote_include_paths", strings(quote_include_directories));
        variables.strings("system_include_paths", strings(system_include_directories));
        variables.strings("framework_include_paths", strings(framework_include_directories));
        variables.strings("preprocessor_defines", strings(preprocessor_defines));
        if use_pic {
            variables.string("pic", "");
        }
        extend_variables(&mut variables, variables_extension)?;
        Ok(VariablesValue(Arc::new(variables)))
    }

    fn create_link_variables<'v>(
        this: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] library_search_directories: Option<Value<'v>>,
        #[starlark(require = named)] runtime_library_search_directories: Option<Value<'v>>,
        #[starlark(require = named)] user_link_flags: Option<Value<'v>>,
        #[starlark(require = named)] output_file: Option<Value<'v>>,
        #[starlark(require = named)] param_file: Option<Value<'v>>,
        #[starlark(require = named, default = true)] is_using_linker: bool,
        #[starlark(require = named, default = false)] is_linking_dynamic_library: bool,
        #[starlark(require = named, default = true)] must_keep_debug: bool,
        #[starlark(require = named, default = false)] use_test_only_flags: bool,
        #[starlark(require = named, default = true)] is_static_linking_mode: bool,
    ) -> anyhow::Result<VariablesValue> {
        let _ = (this, feature_configuration, is_static_linking_mode, use_test_only_flags);
        let toolchain = toolchain(cc_toolchain)?;
        let link = LinkVariables {
            library_search_directories: strings(library_search_directories),
            runtime_library_search_directories: strings(runtime_library_search_directories),
            user_link_flags: strings(user_link_flags),
            output: output_file.filter(|value| !value.is_none()).map(crate::args::stringify),
            param_file: param_file.filter(|value| !value.is_none()).map(crate::args::stringify),
            is_using_linker,
            is_linking_dynamic_library,
            must_keep_debug,
            libraries_to_link: None,
        };
        Ok(VariablesValue(Arc::new(link.variables(&toolchain.config))))
    }

    fn create_compilation_context<'v>(
        this: Value<'v>,
        #[starlark(require = named)] headers: Option<Value<'v>>,
        #[starlark(require = named)] system_includes: Option<Value<'v>>,
        #[starlark(require = named)] includes: Option<Value<'v>>,
        #[starlark(require = named)] quote_includes: Option<Value<'v>>,
        #[starlark(require = named)] framework_includes: Option<Value<'v>>,
        #[starlark(require = named)] defines: Option<Value<'v>>,
        #[starlark(require = named)] local_defines: Option<Value<'v>>,
        #[starlark(require = named)] direct_textual_headers: Option<Value<'v>>,
        #[starlark(require = named)] direct_public_headers: Option<Value<'v>>,
        #[starlark(require = named)] direct_private_headers: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, direct_textual_headers);
        let fields = CompilationFields {
            headers: as_depset(headers, heap),
            direct_public_headers: items(direct_public_headers),
            direct_private_headers: items(direct_private_headers),
            includes: as_depset(includes, heap),
            quote_includes: as_depset(quote_includes, heap),
            system_includes: as_depset(system_includes, heap),
            framework_includes: as_depset(framework_includes, heap),
            defines: as_depset(defines, heap),
            local_defines: as_depset(local_defines, heap),
        };
        Ok(compilation_context(fields, heap))
    }

    fn merge_compilation_contexts<'v>(this: Value<'v>, compilation_contexts: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(super::api::merge_compilation_contexts(&items(compilation_contexts), heap))
    }

    fn create_compilation_outputs<'v>(
        this: Value<'v>,
        #[starlark(require = named)] objects: Option<Value<'v>>,
        #[starlark(require = named)] pic_objects: Option<Value<'v>>,
        #[starlark(require = named)] lto_compilation_context: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, lto_compilation_context);
        Ok(heap.alloc(CcStruct::new(
            "CompilationOutputs",
            vec![
                ("objects", heap.alloc(AllocList(items(objects)))),
                ("pic_objects", heap.alloc(AllocList(items(pic_objects)))),
            ],
        )))
    }

    fn create_linking_context<'v>(
        this: Value<'v>,
        #[starlark(require = named)] linker_inputs: Option<Value<'v>>,
        #[starlark(require = named)] libraries_to_link: Option<Value<'v>>,
        #[starlark(require = named)] user_link_flags: Option<Value<'v>>,
        #[starlark(require = named)] additional_inputs: Option<Value<'v>>,
        #[starlark(require = named)] extra_link_time_library: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, extra_link_time_library);
        let heap = eval.heap();
        if let Some(inputs) = linker_inputs.filter(|value| !value.is_none()) {
            let transitive = match Depset::from_value(inputs) {
                Some(_) => vec![inputs],
                None => vec![depset(items(Some(inputs)), Vec::new(), Order::Topological, heap)],
            };
            return Ok(linking_context(Vec::new(), transitive, heap));
        }
        let owner = heap.alloc(bazel_starlark::label_value::LabelValue(RuleState::get(eval)?.label));
        let input = linker_input(owner, items(libraries_to_link), strings(user_link_flags), items(additional_inputs), heap);
        Ok(linking_context(vec![input], Vec::new(), heap))
    }

    fn create_linker_input<'v>(
        this: Value<'v>,
        #[starlark(require = named)] owner: Value<'v>,
        #[starlark(require = named)] libraries: Option<Value<'v>>,
        #[starlark(require = named)] user_link_flags: Option<Value<'v>>,
        #[starlark(require = named)] additional_inputs: Option<Value<'v>>,
        #[starlark(require = named)] linkstamps: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, linkstamps);
        Ok(linker_input(owner, items(libraries), strings(user_link_flags), items(additional_inputs), heap))
    }

    fn merge_linking_contexts<'v>(this: Value<'v>, linking_contexts: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(super::api::merge_linking_contexts(&items(linking_contexts), heap))
    }

    fn create_library_to_link<'v>(
        this: Value<'v>,
        #[starlark(require = named)] actions: Value<'v>,
        #[starlark(require = named)] feature_configuration: Option<Value<'v>>,
        #[starlark(require = named)] cc_toolchain: Option<Value<'v>>,
        #[starlark(require = named)] static_library: Option<Value<'v>>,
        #[starlark(require = named)] pic_static_library: Option<Value<'v>>,
        #[starlark(require = named)] dynamic_library: Option<Value<'v>>,
        #[starlark(require = named)] interface_library: Option<Value<'v>>,
        #[starlark(require = named)] pic_objects: Option<Value<'v>>,
        #[starlark(require = named)] objects: Option<Value<'v>>,
        #[starlark(require = named, default = false)] alwayslink: bool,
        #[starlark(require = named)] dynamic_library_symlink_path: Option<Value<'v>>,
        #[starlark(require = named)] interface_library_symlink_path: Option<Value<'v>>,
        #[starlark(require = named)] must_keep_debug: Option<Value<'v>>,
        #[starlark(require = named)] lto_compilation_context: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, actions, feature_configuration, cc_toolchain, dynamic_library_symlink_path, interface_library_symlink_path, must_keep_debug, lto_compilation_context);
        let file = |value: Option<Value<'v>>| value.filter(|value| FileValue::artifact(*value).is_some());
        let files = LibraryFiles {
            static_library: file(static_library),
            pic_static_library: file(pic_static_library),
            dynamic_library: file(dynamic_library),
            interface_library: file(interface_library),
            objects: items(objects),
            pic_objects: items(pic_objects),
            alwayslink,
        };
        Ok(library_to_link(files, heap))
    }

    fn create_linking_context_from_compilation_outputs<'v>(
        this: Value<'v>,
        #[starlark(require = named)] actions: Value<'v>,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] language: Option<Value<'v>>,
        #[starlark(require = named, default = false)] disallow_static_libraries: bool,
        #[starlark(require = named, default = false)] disallow_dynamic_library: bool,
        #[starlark(require = named)] compilation_outputs: Value<'v>,
        #[starlark(require = named)] linking_contexts: Option<Value<'v>>,
        #[starlark(require = named)] user_link_flags: Option<Value<'v>>,
        #[starlark(require = named, default = false)] alwayslink: bool,
        #[starlark(require = named)] additional_inputs: Option<Value<'v>>,
        #[starlark(require = named)] variables_extension: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, actions, language, disallow_dynamic_library, variables_extension);
        let heap = eval.heap();
        let state = RuleState::get(eval)?;
        let configuration = unpack_features(feature_configuration)?;
        let toolchain = toolchain(cc_toolchain)?;
        let objects: Vec<_> = items(CcStruct::field_of(compilation_outputs, "CompilationOutputs", "objects")).into_iter().filter_map(FileValue::artifact).collect();
        let pic_objects: Vec<_> = items(CcStruct::field_of(compilation_outputs, "CompilationOutputs", "pic_objects")).into_iter().filter_map(FileValue::artifact).collect();
        let mut library = LibraryFiles {
            objects: objects.iter().map(|artifact| heap.alloc(FileValue(*artifact))).collect(),
            pic_objects: pic_objects.iter().map(|artifact| heap.alloc(FileValue(*artifact))).collect(),
            alwayslink,
            ..Default::default()
        };
        if !disallow_static_libraries && !(objects.is_empty() && pic_objects.is_empty()) {
            let (archive, pic) = match pic_objects.is_empty() {
                true => (&objects, false),
                false => (&pic_objects, true),
            };
            let output = super::rules::archive(state, toolchain, configuration, name, archive, alwayslink)?;
            match pic {
                true => library.pic_static_library = Some(heap.alloc(FileValue(output))),
                false => library.static_library = Some(heap.alloc(FileValue(output))),
            }
        }
        let owner = heap.alloc(bazel_starlark::label_value::LabelValue(state.label));
        let input = linker_input(owner, vec![library_to_link(library, heap)], strings(user_link_flags), items(additional_inputs), heap);
        let transitive: Vec<Value<'v>> = items(linking_contexts).into_iter().filter_map(linker_inputs_of).collect();
        let context = linking_context(vec![input], transitive, heap);
        let outputs = heap.alloc(CcStruct::new("LinkingOutputs", vec![("library_to_link", Value::new_none()), ("executable", Value::new_none())]));
        Ok(heap.alloc(AllocTuple([context, outputs])))
    }

    fn merge_cc_infos<'v>(
        this: Value<'v>,
        #[starlark(require = named)] direct_cc_infos: Option<Value<'v>>,
        #[starlark(require = named)] cc_infos: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let mut all = items(direct_cc_infos);
        all.extend(items(cc_infos));
        Ok(super::api::merge_cc_infos(&all, heap))
    }

    fn create_debug_context<'v>(this: Value<'v>, compilation_outputs: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = (this, compilation_outputs);
        Ok(heap.alloc(CcStruct::new("DebugContext", vec![("files", empty_depset(heap)), ("pic_files", empty_depset(heap))])))
    }

    fn merge_debug_context<'v>(this: Value<'v>, debug_contexts: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = (this, debug_contexts);
        Ok(heap.alloc(CcStruct::new("DebugContext", vec![("files", empty_depset(heap)), ("pic_files", empty_depset(heap))])))
    }

    fn create_cc_toolchain_config_info<'v>(
        this: Value<'v>,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] toolchain_identifier: &str,
        #[starlark(require = named)] features: Option<Value<'v>>,
        #[starlark(require = named)] action_configs: Option<Value<'v>>,
        #[starlark(require = named)] artifact_name_patterns: Option<Value<'v>>,
        #[starlark(require = named)] cxx_builtin_include_directories: Option<Value<'v>>,
        #[starlark(require = named)] host_system_name: Option<&str>,
        #[starlark(require = named)] target_system_name: Option<&str>,
        #[starlark(require = named)] target_cpu: Option<&str>,
        #[starlark(require = named)] target_libc: Option<&str>,
        #[starlark(require = named)] compiler: Option<&str>,
        #[starlark(require = named)] abi_version: Option<&str>,
        #[starlark(require = named)] abi_libc_version: Option<&str>,
        #[starlark(require = named)] tool_paths: Option<Value<'v>>,
        #[starlark(require = named)] make_variables: Option<Value<'v>>,
        #[starlark(require = named)] builtin_sysroot: Option<&str>,
        #[starlark(require = named)] cc_target_os: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<ToolchainConfigValue> {
        let _ = (this, ctx, cc_target_os);
        let state = RuleState::get(eval)?;
        let package_dir = crate::artifact::package_dir(state.label.package).to_string();
        let reader = Reader { heap: eval.heap() };
        let tool_paths = reader.tool_paths(tool_paths, &package_dir);
        let mut toolchain_features = ToolchainFeatures {
            features: items(features).into_iter().map(|feature| reader.feature(feature)).collect(),
            action_configs: items(action_configs).into_iter().map(|config| reader.action_config(config, &package_dir)).collect(),
        };
        CcToolchainConfig::add_legacy_action_configs(&mut toolchain_features, &tool_paths);
        let text = |value: Option<&str>| value.unwrap_or_default().to_string();
        Ok(ToolchainConfigValue(Arc::new(CcToolchainConfig {
            toolchain_identifier: toolchain_identifier.to_string(),
            host_system_name: text(host_system_name),
            target_system_name: text(target_system_name),
            target_cpu: text(target_cpu),
            target_libc: text(target_libc),
            compiler: text(compiler),
            abi_version: text(abi_version),
            abi_libc_version: text(abi_libc_version),
            builtin_sysroot: builtin_sysroot.filter(|sysroot| !sysroot.is_empty()).map(str::to_string),
            cxx_builtin_include_directories: strings(cxx_builtin_include_directories),
            tool_paths,
            make_variables: reader.make_variables(make_variables),
            artifact_name_patterns: reader.artifact_name_patterns(artifact_name_patterns),
            features: Arc::new(toolchain_features),
        })))
    }

    fn compile<'v>(this: Value<'v>, #[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = (this, kwargs);
        anyhow::bail!("cc_common.compile isn't supported by bb yet")
    }

    fn link<'v>(this: Value<'v>, #[starlark(kwargs)] kwargs: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = (this, kwargs);
        anyhow::bail!("cc_common.link isn't supported by bb yet")
    }
}

/// Variables every action of a toolchain has, such as `sysroot`.
pub fn toolchain_variables(config: &CcToolchainConfig) -> Variables {
    let mut variables = Variables::default();
    if let Some(sysroot) = &config.builtin_sysroot {
        variables.string("sysroot", sysroot.clone());
    }
    variables
}

/// Add `variables_extension`, a dict of strings and lists of strings.
fn extend_variables(variables: &mut Variables, extension: Option<Value<'_>>) -> anyhow::Result<()> {
    let Some(dict) = extension.and_then(DictRef::from_value) else {
        return Ok(());
    };
    for (name, value) in dict.iter() {
        let Some(name) = name.unpack_str() else {
            continue;
        };
        let value = match value.unpack_str() {
            Some(text) => Variable::String(text.to_string()),
            None => Variable::Sequence(strings(Some(value)).into_iter().map(Variable::String).collect()),
        };
        variables.set(name, value);
    }
    Ok(())
}

/// The variables of a link action.
pub struct LinkVariables {
    pub library_search_directories: Vec<String>,
    pub runtime_library_search_directories: Vec<String>,
    pub user_link_flags: Vec<String>,
    pub output: Option<String>,
    pub param_file: Option<String>,
    pub is_using_linker: bool,
    pub is_linking_dynamic_library: bool,
    pub must_keep_debug: bool,
    pub libraries_to_link: Option<Vec<Variable>>,
}

impl LinkVariables {
    pub fn variables(self, config: &CcToolchainConfig) -> Variables {
        let mut variables = toolchain_variables(config);
        if !self.library_search_directories.is_empty() {
            variables.strings("library_search_directories", self.library_search_directories);
        }
        if !self.runtime_library_search_directories.is_empty() {
            variables.strings("runtime_library_search_directories", self.runtime_library_search_directories);
        }
        variables.strings("user_link_flags", self.user_link_flags);
        if let Some(output) = self.output {
            variables.string("output_execpath", output);
        }
        if let Some(param_file) = self.param_file {
            variables.string("linker_param_file", param_file);
        }
        variables.set("is_using_linker", Variable::Bool(self.is_using_linker));
        variables.set("is_linking_dynamic_library", Variable::Bool(self.is_linking_dynamic_library));
        if !self.must_keep_debug {
            variables.string("strip_debug_symbols", "");
        }
        if let Some(libraries) = self.libraries_to_link {
            variables.set("libraries_to_link", Variable::Sequence(libraries));
        }
        variables
    }
}

/// The structure of a library or object file in `libraries_to_link`.
pub fn library_variable(kind: &str, name: &str, objects: &[String], whole_archive: bool) -> Variable {
    let mut fields = BTreeMap::new();
    fields.insert("type".to_string(), Variable::String(kind.to_string()));
    fields.insert("name".to_string(), Variable::String(name.to_string()));
    fields.insert("is_whole_archive".to_string(), Variable::Bool(whole_archive));
    if !objects.is_empty() {
        fields.insert("object_files".to_string(), Variable::Sequence(objects.iter().cloned().map(Variable::String).collect()));
    }
    Variable::Structure(fields)
}

/// Add `cc_common`, `CcInfo` and `CcToolchainConfigInfo` to `.bzl` files.
pub fn register(builder: &mut GlobalsBuilder) {
    let cc_common = builder.alloc(CcCommon);
    builder.set("cc_common", cc_common);
    let cc_info = builder.alloc(NativeProvider {
        name: "CcInfo",
        constructor: Some(construct_cc_info),
    });
    builder.set("CcInfo", cc_info);
    if let Ok(provider) = crate::providers::builtin("CcToolchainConfigInfo") {
        builder.set("CcToolchainConfigInfo", provider.unpack_frozen().expect("builtin providers are frozen"));
    }
}
