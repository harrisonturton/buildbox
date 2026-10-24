//! `cc_internal`: the native C++ API Bazel 9's `@_builtins` and rules_cc's
//! Starlark C++ rules are written against, as Bazel's
//! `CcStarlarkInternal` implements it.

use allocative::Allocative;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::tuple::AllocTuple;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkValue, Value, ValueLike, starlark_value};
use starlark::{StarlarkPagablePanic, starlark_simple_value};
use super::config::{CcToolchainConfig, Reader};
use super::features::{ToolchainFeatures, Variables};
use super::values::{FeatureConfigurationValue, VariablesValue};
use crate::file_value::FileValue;
use crate::inputs::{InputSet, InputSets};
use bazel_starlark::depset::Depset;
use bazel_starlark::label_value::LabelValue;
use crate::state::RuleState;
use std::fmt;
use std::sync::Arc;

/// `_builtins.internal.cc_internal`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct CcInternal;

starlark_simple_value!(CcInternal);

impl fmt::Display for CcInternal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<cc_internal>")
    }
}

#[starlark_value(type = "cc_internal")]
impl<'v> StarlarkValue<'v> for CcInternal {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("cc_internal", cc_internal_methods);
        Some(METHODS.methods())
    }

    /// Members bb doesn't implement are functions that fail when called,
    /// so that files using others still load.
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(Unsupported(format!("cc_internal.{attribute}"))))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

#[starlark_module]
fn cc_internal_methods(builder: &mut MethodsBuilder) {
    /// Bazel restricts private APIs to allowlisted packages; bb lets every
    /// package use them.
    fn check_private_api<'v>(this: Value<'v>, #[starlark(require = named)] allowlist: Value<'v>, #[starlark(require = named)] depth: Option<i32>) -> anyhow::Result<NoneType> {
        let _ = (this, allowlist, depth);
        Ok(NoneType)
    }

    /// `value`, as Bazel makes an immutable copy of a list or dict. Values
    /// are frozen when a rule's analysis ends anyway, so the value itself
    /// serves.
    fn freeze<'v>(this: Value<'v>, #[starlark(require = pos)] value: Value<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(value)
    }

    /// Bazel checks that a function is defined at the top level of its file;
    /// bb takes it on trust.
    fn check_toplevel<'v>(this: Value<'v>, #[starlark(require = pos)] r#fn: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = (this, r#fn);
        Ok(NoneType)
    }

    /// The operating system of the platform the rule's actions run on, as
    /// Bazel names it, such as `LINUX`. bb runs actions on this machine.
    fn exec_os<'v>(this: Value<'v>, #[starlark(require = pos)] ctx: Value<'v>) -> anyhow::Result<&'static str> {
        let _ = (this, ctx);
        Ok(host_os())
    }

    /// The `ctx` whose `actions` these are.
    fn actions2ctx_cheat<'v>(this: Value<'v>, #[starlark(require = pos)] actions: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        let _ = (this, actions);
        crate::build_ctx::rule_ctx(eval)
    }

    /// The kind of rule being analyzed, such as `cc_library`.
    fn rule_class<'v>(this: Value<'v>, #[starlark(require = pos)] ctx: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<String> {
        let _ = (this, ctx);
        Ok(RuleState::get(eval)?.kind.as_str().to_string())
    }

    /// The aspect being applied, if any. bb doesn't run aspects.
    fn aspect_class<'v>(this: Value<'v>, #[starlark(require = pos)] ctx: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = (this, ctx);
        Ok(NoneType)
    }

    /// Whether the package sets `default_hdrs_check`, which bb doesn't
    /// record: as if it doesn't.
    fn is_package_headers_checking_mode_set<'v>(this: Value<'v>, #[starlark(require = named)] ctx: Value<'v>) -> anyhow::Result<bool> {
        let _ = (this, ctx);
        Ok(false)
    }

    fn package_headers_checking_mode<'v>(this: Value<'v>, #[starlark(require = named)] ctx: Value<'v>) -> anyhow::Result<&'static str> {
        let _ = (this, ctx);
        Ok("")
    }

    fn is_package_headers_checking_mode_set_for_aspect<'v>(this: Value<'v>, #[starlark(require = named)] ctx: Value<'v>) -> anyhow::Result<bool> {
        let _ = (this, ctx);
        Ok(false)
    }

    fn package_headers_checking_mode_for_aspect<'v>(this: Value<'v>, #[starlark(require = named)] ctx: Value<'v>) -> anyhow::Result<&'static str> {
        let _ = (this, ctx);
        Ok("")
    }

    /// Whether an artifact is a directory, which Bazel calls a tree.
    fn is_tree_artifact<'v>(this: Value<'v>, #[starlark(require = pos)] artifact: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        let artifact = FileValue::artifact(artifact).ok_or_else(|| anyhow::anyhow!("is_tree_artifact: expected a File, got {}", artifact.get_type()))?;
        Ok(artifact.is_directory)
    }

    /// The directory output names are prefixed with: `arc` or `non_arc` for
    /// Objective-C, by the configuration's mnemonic or `purpose`, or none.
    fn compute_output_name_prefix_dir<'v>(
        this: Value<'v>,
        #[starlark(require = named)] configuration: Value<'v>,
        #[starlark(require = named)] purpose: Value<'v>,
    ) -> anyhow::Result<&'static str> {
        let _ = (this, configuration);
        let Some(purpose) = purpose.unpack_str() else {
            return Ok("");
        };
        let prefix = match purpose {
            purpose if purpose.ends_with("_non_objc_arc") => "non_arc",
            purpose if purpose.ends_with("_objc_arc") => "arc",
            _ => "",
        };
        Ok(prefix)
    }

    /// The items of `seq`, as a tuple. Bazel interns them to save memory.
    fn intern_seq<'v>(this: Value<'v>, #[starlark(require = pos)] seq: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let items = bazel_starlark::convert::items(seq).ok_or_else(|| anyhow::anyhow!("intern_seq: expected a sequence, got {}", seq.get_type()))?;
        Ok(heap.alloc(AllocTuple(items)))
    }

    /// A toolchain's features and action configs, from its
    /// `CcToolchainConfigInfo`, with relative tool paths taken as relative
    /// to `tools_directory`.
    fn cc_toolchain_features<'v>(
        this: Value<'v>,
        #[starlark(require = named)] toolchain_config_info: Value<'v>,
        #[starlark(require = named)] tools_directory: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<CcToolchainFeaturesValue> {
        let _ = this;
        Ok(CcToolchainFeaturesValue(Arc::new(toolchain_config(toolchain_config_info, tools_directory, heap))))
    }

    /// Build variables from a dict of names to values.
    fn cc_toolchain_variables<'v>(this: Value<'v>, #[starlark(require = named)] vars: Value<'v>, heap: Heap<'v>) -> anyhow::Result<VariablesValue> {
        let _ = this;
        Ok(VariablesValue(Arc::new(super::variables::from_dict(vars, heap)?)))
    }

    /// `parent`'s variables with those of `variables` on top, later ones
    /// taking precedence.
    fn combine_cc_toolchain_variables<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] parent: Value<'v>,
        #[starlark(args)] variables: starlark::values::tuple::UnpackTuple<Value<'v>>,
    ) -> anyhow::Result<VariablesValue> {
        let _ = this;
        let parent = toolchain_variables(parent)?;
        let children: Vec<&Variables> = variables.items.iter().map(|value| toolchain_variables(*value)).collect::<anyhow::Result<_>>()?;
        Ok(VariablesValue(Arc::new(super::variables::combine(parent, &children))))
    }

    /// A sequence of strings for a variable. Bazel interns it, to share it
    /// among targets; bb copies it.
    fn intern_string_sequence_variable_value<'v>(this: Value<'v>, #[starlark(require = pos)] string_sequence: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let strings = bazel_starlark::convert::strings(string_sequence).ok_or_else(|| anyhow::anyhow!("intern_string_sequence_variable_value: expected strings"))?;
        Ok(heap.alloc(AllocList(strings)))
    }

    /// The name of an output of `category`, such as `PIC_FILE`, from its
    /// name without the toolchain's prefix and extension for the category.
    fn get_artifact_name_for_category<'v>(
        this: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        #[starlark(require = named)] output_name: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<String> {
        let _ = this;
        let (prefix, extension) = artifact_name_pattern(&toolchain_features_of(cc_toolchain, heap)?.0, category)?;
        let (directory, base) = match output_name.rsplit_once('/') {
            Some((directory, base)) => (Some(directory), base),
            None => (None, output_name),
        };
        let name = format!("{prefix}{base}{extension}");
        Ok(match directory {
            Some(directory) => format!("{directory}/{name}"),
            None => name,
        })
    }

    /// The extension of outputs of `category` the toolchain uses.
    fn get_artifact_name_extension_for_category<'v>(
        this: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] category: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<String> {
        let _ = this;
        let (_, extension) = artifact_name_pattern(&toolchain_features_of(cc_toolchain, heap)?.0, category)?;
        Ok(extension)
    }

    /// An output of compiling for `label`: `output_name` in its package's
    /// `_objs/<name>` directory.
    fn declare_compile_output_file<'v>(
        this: Value<'v>,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] label: Value<'v>,
        #[starlark(require = named)] output_name: &str,
        #[starlark(require = named)] configuration: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let _ = (this, ctx, configuration);
        let label = label.downcast_ref::<LabelValue>().ok_or_else(|| anyhow::anyhow!("declare_compile_output_file: label must be a Label"))?.0;
        let path = format!("{}/_objs/{}/{output_name}", crate::artifact::package_dir(label.package), label.name);
        Ok(FileValue(RuleState::get(eval)?.declare_at_root(&path)?))
    }

    /// A file named `output_name` beside `object_file`.
    fn declare_other_output_file<'v>(
        this: Value<'v>,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] output_name: &str,
        #[starlark(require = named)] object_file: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let _ = (this, ctx);
        let object = FileValue::artifact(object_file).ok_or_else(|| anyhow::anyhow!("declare_other_output_file: object_file must be a File"))?;
        let relative = object.exec_path.strip_prefix(object.root()).unwrap_or(object.exec_path);
        let path = match relative.parent().filter(|parent| !parent.is_root()) {
            Some(directory) => format!("{directory}/{output_name}"),
            None => output_name.to_string(),
        };
        Ok(FileValue(RuleState::get(eval)?.declare_at_root(&path)?))
    }

    /// Register the action compiling `source` into `output_file`, with
    /// the command line `feature_configuration` gives from
    /// `compile_build_variables`.
    fn create_cc_compile_action<'v>(
        this: Value<'v>,
        #[starlark(require = named)] action_construction_context: Value<'v>,
        #[starlark(require = named)] cc_compilation_context: Value<'v>,
        #[starlark(require = named)] cc_toolchain: Value<'v>,
        #[starlark(require = named)] configuration: Value<'v>,
        #[starlark(require = named)] copts_filter: Option<Value<'v>>,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] source: Value<'v>,
        #[starlark(require = named)] additional_compilation_inputs: Option<Value<'v>>,
        #[starlark(require = named)] additional_compilation_inputs_set: Option<Value<'v>>,
        #[starlark(require = named)] additional_include_scanning_roots: Option<Value<'v>>,
        #[starlark(require = named)] output_file: Value<'v>,
        #[starlark(require = named)] dotd_file: Option<Value<'v>>,
        #[starlark(require = named)] diagnostics_file: Option<Value<'v>>,
        #[starlark(require = named)] gcno_file: Option<Value<'v>>,
        #[starlark(require = named)] dwo_file: Option<Value<'v>>,
        #[starlark(require = named)] lto_indexing_file: Option<Value<'v>>,
        #[starlark(require = named)] use_pic: Option<Value<'v>>,
        #[starlark(require = named)] compile_build_variables: Value<'v>,
        #[starlark(require = named)] cache_key_inputs: Option<Value<'v>>,
        #[starlark(require = named)] build_info_header_files: Option<Value<'v>>,
        #[starlark(require = named)] additional_prunable_headers: Option<Value<'v>>,
        #[starlark(require = named)] action_name: Option<Value<'v>>,
        #[starlark(require = named)] progress_message_prefix: Option<Value<'v>>,
        #[starlark(require = named)] should_scan_includes: Option<Value<'v>>,
        #[starlark(require = named)] shareable: Option<Value<'v>>,
        #[starlark(require = named)] module_files: Option<Value<'v>>,
        #[starlark(require = named)] modmap_file: Option<Value<'v>>,
        #[starlark(require = named)] modmap_input_file: Option<Value<'v>>,
        #[starlark(require = named)] additional_outputs: Option<Value<'v>>,
        #[starlark(require = named)] needs_include_validation: Option<Value<'v>>,
        #[starlark(require = named)] toolchain_type: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, action_construction_context, configuration, copts_filter, additional_include_scanning_roots, use_pic);
        let _ = (cache_key_inputs, should_scan_includes, shareable, needs_include_validation, toolchain_type);
        let state = RuleState::get(eval)?;
        let heap = eval.heap();
        let source = FileValue::artifact(source).ok_or_else(|| anyhow::anyhow!("create_cc_compile_action: source must be a File"))?;
        let action_name = optional(action_name).and_then(|name| name.unpack_str()).map(str::to_string);
        let implied_action = match &action_name {
            Some(name) => name.clone(),
            None => super::compile::action_name(&source)?.to_string(),
        };
        let mut sets = state.input_sets.borrow_mut();
        let mut inputs = vec![
            sets.convert(field(cc_compilation_context, "headers", heap).unwrap_or(Value::new_none()), "headers")?,
            sets.convert(field(cc_compilation_context, "_non_code_inputs", heap).unwrap_or(Value::new_none()), "_non_code_inputs")?,
            toolchain_compile_files(&mut sets, cc_toolchain, &implied_action, heap)?,
            module_map_files(cc_compilation_context, heap)?,
        ];
        for (value, what) in [
            (additional_compilation_inputs, "additional_compilation_inputs"),
            (additional_compilation_inputs_set, "additional_compilation_inputs_set"),
            (additional_prunable_headers, "additional_prunable_headers"),
            (build_info_header_files, "build_info_header_files"),
            (module_files, "module_files"),
            (modmap_file, "modmap_file"),
            (modmap_input_file, "modmap_input_file"),
            (field(cc_toolchain, "_builtin_include_files", heap), "_builtin_include_files"),
        ] {
            inputs.push(sets.convert(optional(value).unwrap_or(Value::new_none()), what)?);
        }
        drop(sets);
        let mut outputs = vec![FileValue::artifact(output_file).ok_or_else(|| anyhow::anyhow!("create_cc_compile_action: output_file must be a File"))?];
        for file in [dotd_file, diagnostics_file, gcno_file, dwo_file, lto_indexing_file].into_iter().filter_map(optional) {
            outputs.extend(FileValue::artifact(file));
        }
        outputs.extend(crate::actions::files(optional(additional_outputs), "additional_outputs")?);
        let configuration = feature_configuration.downcast_ref::<FeatureConfigurationValue>().ok_or_else(|| anyhow::anyhow!("create_cc_compile_action: feature_configuration must be a FeatureConfiguration"))?;
        let compile = super::compile::Compile {
            source,
            action_name,
            progress_message_prefix: optional(progress_message_prefix).and_then(|prefix| prefix.unpack_str()).map(str::to_string),
            inputs: InputSet::new(Vec::new(), inputs),
            outputs,
        };
        super::compile::register(state, &configuration.0, toolchain_variables(compile_build_variables)?, compile)?;
        Ok(NoneType)
    }

    /// `actions` as link actions use them, declaring files relative to the
    /// output directory, which bb's `ctx.actions` already can.
    fn wrap_link_actions<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] actions: Value<'v>,
        #[starlark(require = pos)] build_configuration: Option<Value<'v>>,
        #[starlark(require = pos)] sharable_artifacts: Option<bool>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, build_configuration, sharable_artifacts);
        Ok(actions)
    }

    /// The soname of the shared library at `path`.
    fn dynamic_library_soname<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] actions: Value<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] preserve_name: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let _ = (this, actions);
        Ok(super::solib::soname(path, preserve_name, &RuleState::get(eval)?.config.output_directory))
    }

    /// A symlink to the shared library `library` in `solib_directory`.
    fn dynamic_library_symlink<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] actions: Value<'v>,
        #[starlark(require = pos)] library: Value<'v>,
        #[starlark(require = pos)] solib_directory: &str,
        #[starlark(require = pos)] preserve_name: bool,
        #[starlark(require = pos)] prefix_consumer: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let _ = (this, actions);
        let library = FileValue::artifact(library).ok_or_else(|| anyhow::anyhow!("dynamic_library_symlink: library must be a File"))?;
        let state = RuleState::get(eval)?;
        Ok(FileValue(super::solib::library_symlink(state, library, solib_directory, preserve_name, prefix_consumer)?))
    }

    /// A symlink to the C++ runtime library `artifact` in the solib
    /// directory: `runtime_solib_dir_base` if given, or `solib_directory`.
    fn solib_symlink_action<'v>(
        this: Value<'v>,
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] artifact: Value<'v>,
        #[starlark(require = named)] solib_directory: &str,
        #[starlark(require = named)] runtime_solib_dir_base: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let _ = (this, ctx);
        let library = FileValue::artifact(artifact).ok_or_else(|| anyhow::anyhow!("solib_symlink_action: artifact must be a File"))?;
        let directory = optional(runtime_solib_dir_base).and_then(|base| base.unpack_str()).unwrap_or(solib_directory);
        Ok(FileValue(super::solib::runtime_symlink(RuleState::get(eval)?, library, directory)?))
    }

    /// `filename`, or if it's too long to be one, its hash with its
    /// extension.
    fn maybe_hash_preserve_extension<'v>(this: Value<'v>, #[starlark(require = pos)] filename: &str) -> anyhow::Result<String> {
        let _ = this;
        Ok(super::solib::hash_if_too_long(filename))
    }

    /// The arguments of the link `action_name`, after its tool, in a params
    /// file of `parameter_file_type` if one's given.
    fn get_link_args<'v>(
        this: Value<'v>,
        #[starlark(require = named)] action_name: &str,
        #[starlark(require = named)] feature_configuration: Value<'v>,
        #[starlark(require = named)] build_variables: Value<'v>,
        #[starlark(require = named)] parameter_file_type: Option<Value<'v>>,
        #[starlark(require = named)] param_file_name: Option<Value<'v>>,
    ) -> anyhow::Result<crate::args::Args> {
        let _ = (this, param_file_name);
        let configuration = feature_configuration.downcast_ref::<FeatureConfigurationValue>().ok_or_else(|| anyhow::anyhow!("get_link_args: feature_configuration must be a FeatureConfiguration"))?;
        let parameter_file_type = optional(parameter_file_type).and_then(|value| value.unpack_str());
        let args = super::link::args(&configuration.0, action_name, toolchain_variables(build_variables)?, parameter_file_type)?;
        Ok(crate::args::Args::from_state(args))
    }

    /// The `--per_file_copt`s for a source file, which bb doesn't take:
    /// none.
    fn per_file_copts<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] cpp_configuration: Value<'v>,
        #[starlark(require = pos)] source_file: Value<'v>,
        #[starlark(require = pos)] label: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, cpp_configuration, source_file, label);
        Ok(heap.alloc(AllocList::EMPTY))
    }

    /// The `--per_file_lto_backend_opt`s for an object file: none.
    fn collect_per_file_lto_backend_opts<'v>(
        this: Value<'v>,
        #[starlark(require = pos)] cpp_config: Value<'v>,
        #[starlark(require = pos)] obj: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (this, cpp_config, obj);
        Ok(heap.alloc(AllocList::EMPTY))
    }

    /// A target's headers as C++ modules see them. bb doesn't build header
    /// modules, so this only holds what it's given.
    fn create_header_info<'v>(
        this: Value<'v>,
        #[starlark(require = named)] header_module: Option<Value<'v>>,
        #[starlark(require = named)] pic_header_module: Option<Value<'v>>,
        #[starlark(require = named)] modular_public_headers: Option<Value<'v>>,
        #[starlark(require = named)] modular_private_headers: Option<Value<'v>>,
        #[starlark(require = named)] textual_headers: Option<Value<'v>>,
        #[starlark(require = named)] separate_module_headers: Option<Value<'v>>,
        #[starlark(require = named)] separate_module: Option<Value<'v>>,
        #[starlark(require = named)] separate_pic_module: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let none = Value::new_none();
        let empty = || heap.alloc(AllocList::EMPTY);
        let fields = HeaderInfoFields {
            header_module: header_module.unwrap_or(none),
            pic_header_module: pic_header_module.unwrap_or(none),
            modular_public_headers: modular_public_headers.unwrap_or_else(empty),
            modular_private_headers: modular_private_headers.unwrap_or_else(empty),
            textual_headers: textual_headers.unwrap_or_else(empty),
            separate_module_headers: separate_module_headers.unwrap_or_else(empty),
            separate_module: separate_module.unwrap_or(none),
            separate_pic_module: separate_pic_module.unwrap_or(none),
        };
        Ok(header_info(fields, empty(), empty(), heap))
    }

    /// `header_info` with the header information of its dependencies.
    fn create_header_info_with_deps<'v>(
        this: Value<'v>,
        #[starlark(require = named)] header_info: Value<'v>,
        #[starlark(require = named)] deps: Option<Value<'v>>,
        #[starlark(require = named)] merged_deps: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        let field = |name: &str| header_info.get_attr(name, heap).ok().flatten().unwrap_or(Value::new_none());
        let fields = HeaderInfoFields {
            header_module: field("header_module"),
            pic_header_module: field("pic_header_module"),
            modular_public_headers: field("modular_public_headers"),
            modular_private_headers: field("modular_private_headers"),
            textual_headers: field("textual_headers"),
            separate_module_headers: field("separate_module_headers"),
            separate_module: field("separate_module"),
            separate_pic_module: field("separate_pic_module"),
        };
        let empty = || heap.alloc(AllocList::EMPTY);
        Ok(self::header_info(fields, deps.unwrap_or_else(empty), merged_deps.unwrap_or_else(empty), heap))
    }
}

/// What a `header_info` holds besides its dependencies'.
struct HeaderInfoFields<'v> {
    header_module: Value<'v>,
    pic_header_module: Value<'v>,
    modular_public_headers: Value<'v>,
    modular_private_headers: Value<'v>,
    textual_headers: Value<'v>,
    separate_module_headers: Value<'v>,
    separate_module: Value<'v>,
    separate_pic_module: Value<'v>,
}

/// A `header_info`, as a struct of its fields.
fn header_info<'v>(fields: HeaderInfoFields<'v>, deps: Value<'v>, merged_deps: Value<'v>, heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocStruct([
        ("header_module", fields.header_module),
        ("pic_header_module", fields.pic_header_module),
        ("modular_public_headers", fields.modular_public_headers),
        ("modular_private_headers", fields.modular_private_headers),
        ("textual_headers", fields.textual_headers),
        ("separate_module_headers", fields.separate_module_headers),
        ("separate_module", fields.separate_module),
        ("separate_pic_module", fields.separate_pic_module),
        ("deps", deps),
        ("merged_deps", merged_deps),
    ]))
}

/// The field `name` of `value`, unless it has none or it's `None`.
fn field<'v>(value: Value<'v>, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    value.get_attr(name, heap).ok().flatten().filter(|value| !value.is_none())
}

/// `value`, unless it's absent or `None`.
fn optional<'v>(value: Option<Value<'v>>) -> Option<Value<'v>> {
    value.filter(|value| !value.is_none())
}

/// The toolchain files compiling with `action` needs: the assembler's or
/// the compiler's, or all of them if the toolchain doesn't say.
fn toolchain_compile_files<'v>(sets: &mut InputSets, cc_toolchain: Value<'v>, action: &str, heap: Heap<'v>) -> anyhow::Result<InputSet> {
    let set = match action {
        "assemble" => "_as_files",
        _ => "_compiler_files",
    };
    let files = field(cc_toolchain, set, heap);
    if let Some(files) = files.filter(|files| Depset::from_value(*files).is_none_or(|depset| !depset.is_empty())) {
        return sets.convert(files, set);
    }
    sets.convert(field(cc_toolchain, "_all_files", heap).unwrap_or(Value::new_none()), "_all_files")
}

/// The module map files a compilation reads: its own and its direct
/// dependencies'.
fn module_map_files<'v>(cc_compilation_context: Value<'v>, heap: Heap<'v>) -> anyhow::Result<InputSet> {
    let mut module_maps = Vec::new();
    if let Some(module_map) = field(cc_compilation_context, "_module_map", heap) {
        module_maps.push(module_map);
    }
    if let Some(direct) = field(cc_compilation_context, "_direct_module_maps", heap).and_then(Depset::from_value) {
        module_maps.extend(direct.to_list());
    }
    let mut files = Vec::with_capacity(module_maps.len());
    for module_map in module_maps {
        let file = field(module_map, "file", heap);
        files.extend(file.and_then(FileValue::artifact));
    }
    Ok(InputSet::of(files))
}

/// A toolchain configuration from rules_cc's `CcToolchainConfigInfo`, whose
/// features already include the legacy ones Bazel 8 added natively.
fn toolchain_config<'v>(info: Value<'v>, tools_directory: &str, heap: Heap<'v>) -> CcToolchainConfig {
    let reader = Reader { heap };
    let field = |name: &str| info.get_attr(name, heap).ok().flatten().filter(|value| !value.is_none());
    let text = |name: &str| field(name).and_then(|value| value.unpack_str().map(str::to_string)).unwrap_or_default();
    let items = |name: &str| field(name).and_then(bazel_starlark::convert::items).unwrap_or_default();
    let features = ToolchainFeatures {
        features: items("_features_DO_NOT_USE").into_iter().map(|feature| reader.feature(feature)).collect(),
        action_configs: items("_action_configs_DO_NOT_USE").into_iter().map(|config| reader.action_config(config, tools_directory)).collect(),
    };
    let builtin_sysroot = text("builtin_sysroot");
    CcToolchainConfig {
        toolchain_identifier: text("toolchain_id"),
        host_system_name: String::new(),
        target_system_name: text("target_system_name"),
        target_cpu: text("target_cpu"),
        target_libc: text("target_libc"),
        compiler: text("compiler"),
        abi_version: text("abi_version"),
        abi_libc_version: text("abi_libc_version"),
        builtin_sysroot: (!builtin_sysroot.is_empty()).then_some(builtin_sysroot),
        cxx_builtin_include_directories: items("cxx_builtin_include_directories").into_iter().filter_map(|item| item.unpack_str().map(str::to_string)).collect(),
        tool_paths: reader.tool_paths(field("tool_paths"), tools_directory),
        make_variables: reader.make_variables(field("make_variables")),
        artifact_name_patterns: reader.artifact_name_patterns(field("_artifact_name_patterns_DO_NOT_USE")),
        features: Arc::new(features),
    }
}

/// A toolchain's `CcToolchainFeatures`: its configuration, from which rules
/// configure the features they build with.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct CcToolchainFeaturesValue(#[allocative(skip)] pub Arc<CcToolchainConfig>);

starlark_simple_value!(CcToolchainFeaturesValue);

impl fmt::Display for CcToolchainFeaturesValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcToolchainFeatures {}>", self.0.toolchain_identifier)
    }
}

#[starlark_value(type = "CcToolchainFeatures")]
impl<'v> StarlarkValue<'v> for CcToolchainFeaturesValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("CcToolchainFeatures", toolchain_features_methods);
        Some(METHODS.methods())
    }
}

#[starlark_module]
fn toolchain_features_methods(builder: &mut MethodsBuilder) {
    /// The names of the features and action configs enabled by default.
    fn default_features_and_action_configs<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(AllocList(toolchain_features(this)?.features.defaults())))
    }

    /// The features enabled when `requested_features` are asked for.
    fn configure_features<'v>(this: Value<'v>, #[starlark(require = named)] requested_features: Value<'v>) -> anyhow::Result<FeatureConfigurationValue> {
        let requested = bazel_starlark::convert::strings(requested_features).ok_or_else(|| anyhow::anyhow!("configure_features: requested_features must be strings"))?;
        let configuration = toolchain_features(this)?.features.configure_requested(&requested)?;
        Ok(FeatureConfigurationValue(Arc::new(configuration)))
    }
}

/// Each category of C++ output, as Bazel's `ArtifactCategory` names it,
/// with its default prefix and extension.
const ARTIFACT_CATEGORIES: &[(&str, &str, &str)] = &[
    ("STATIC_LIBRARY", "lib", ".a"),
    ("ALWAYSLINK_STATIC_LIBRARY", "lib", ".lo"),
    ("DYNAMIC_LIBRARY", "lib", ".so"),
    ("EXECUTABLE", "", ""),
    ("INTERFACE_LIBRARY", "lib", ".ifso"),
    ("PIC_FILE", "", ".pic"),
    ("INCLUDED_FILE_LIST", "", ".d"),
    ("SERIALIZED_DIAGNOSTICS_FILE", "", ".dia"),
    ("OBJECT_FILE", "", ".o"),
    ("PIC_OBJECT_FILE", "", ".pic.o"),
    ("CPP_MODULE", "", ".pcm"),
    ("CPP_MODULES_INFO", "", ".CXXModules.json"),
    ("CPP_MODULES_DDI", "", ".ddi"),
    ("CPP_MODULES_MODMAP", "", ".modmap"),
    ("CPP_MODULES_MODMAP_INPUT", "", ".modmap.input"),
    ("GENERATED_ASSEMBLY", "", ".s"),
    ("PROCESSED_HEADER", "", ".processed"),
    ("GENERATED_HEADER", "", ".h"),
    ("PREPROCESSED_C_SOURCE", "", ".i"),
    ("PREPROCESSED_CPP_SOURCE", "", ".ii"),
    ("COVERAGE_DATA_FILE", "", ".gcno"),
    ("CLIF_OUTPUT_PROTO", "", ".opb"),
];

/// The prefix and extension of outputs of `category`: the toolchain's
/// `artifact_name_pattern` for it, or Bazel's default.
fn artifact_name_pattern(config: &CcToolchainConfig, category: &str) -> anyhow::Result<(String, String)> {
    let Some((_, prefix, extension)) = ARTIFACT_CATEGORIES.iter().find(|(name, _, _)| *name == category) else {
        anyhow::bail!("no artifact category named {category}");
    };
    match config.artifact_name_patterns.get(&category.to_ascii_lowercase()) {
        Some((prefix, extension)) => Ok((prefix.clone(), extension.clone())),
        None => Ok((prefix.to_string(), extension.to_string())),
    }
}

/// The features of a `CcToolchainInfo`.
fn toolchain_features_of<'v>(cc_toolchain: Value<'v>, heap: Heap<'v>) -> anyhow::Result<&'v CcToolchainFeaturesValue> {
    let features = cc_toolchain.get_attr("_toolchain_features", heap).ok().flatten();
    match features.and_then(|features| features.downcast_ref::<CcToolchainFeaturesValue>()) {
        Some(features) => Ok(features),
        None => anyhow::bail!("expected a CcToolchainInfo, got {}", cc_toolchain.get_type()),
    }
}

fn toolchain_variables<'v>(value: Value<'v>) -> anyhow::Result<&'v Variables> {
    match value.downcast_ref::<VariablesValue>() {
        Some(variables) => Ok(&variables.0),
        None => anyhow::bail!("expected CcToolchainVariables, got {}", value.get_type()),
    }
}

fn toolchain_features<'v>(value: Value<'v>) -> anyhow::Result<&'v CcToolchainConfig> {
    match value.downcast_ref::<CcToolchainFeaturesValue>() {
        Some(features) => Ok(&features.0),
        None => anyhow::bail!("expected CcToolchainFeatures, got {}", value.get_type()),
    }
}

/// This machine's operating system, as Bazel's `OS` enum names it.
fn host_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "DARWIN",
        "windows" => "WINDOWS",
        "freebsd" => "FREEBSD",
        "openbsd" => "OPENBSD",
        _ => "LINUX",
    }
}

/// A native function bb doesn't implement yet, which fails when called.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Unsupported(String);

starlark_simple_value!(Unsupported);

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<built-in function {}>", self.0)
    }
}

#[starlark_value(type = "builtin_function_or_method")]
impl<'v> StarlarkValue<'v> for Unsupported {
    fn invoke(&self, _me: Value<'v>, _args: &Arguments<'v, '_>, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Err(starlark::Error::new_other(anyhow::anyhow!("{} isn't supported by bb yet", self.0)))
    }
}

/// Add `cc_common` and `cc_internal` to `_builtins.internal`.
pub fn register_builtins_internal(builder: &mut GlobalsBuilder) {
    let cc_common = builder.alloc(super::api::CcCommon);
    builder.set("cc_common", cc_common);
    let cc_internal = builder.alloc(CcInternal);
    builder.set("cc_internal", cc_internal);
}
