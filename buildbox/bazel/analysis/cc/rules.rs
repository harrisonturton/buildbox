//! The C++ rules: toolchains, and the libraries, binaries and tests built
//! with them.

use super::api::{self, CompilationFields, LibraryFiles, LinkVariables};
use super::features::{FeatureConfiguration, Variables};
use super::values::{CcStruct, CcToolchain, ToolchainConfigValue};
use crate::action::{Action, ActionKind, Spawn};
use crate::artifact::{self, Artifact};
use crate::build_ctx::{Prepared, RuleInput};
use crate::file_value::FileValue;
use crate::providers;
use crate::runfiles::Runfiles;
use crate::state::RuleState;
use crate::target_value::TargetValue;
use bazel_label::Label;
use bazel_packages::AttrValue;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::label_value::LabelValue;
use intern::Symbol;
use starlark::collections::SmallMap;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, Value, ValueLike};
use std::sync::Arc;

/// The toolchain type of C++ toolchains.
const CC_TOOLCHAIN_TYPE: &str = "@@bazel_tools//tools/cpp:toolchain_type";

/// The attributes of `cc_toolchain` naming the files its tools need.
const TOOLCHAIN_FILES: &[&str] = &[
    "all_files",
    "compiler_files",
    "linker_files",
    "ar_files",
    "as_files",
    "strip_files",
    "objcopy_files",
    "dwp_files",
    "coverage_files",
    "static_runtime_lib",
    "dynamic_runtime_lib",
];

/// The extensions of source files, and the action that compiles each.
const SOURCE_ACTIONS: &[(&str, &str)] = &[
    ("c", "c-compile"),
    ("cc", "c++-compile"),
    ("cpp", "c++-compile"),
    ("cxx", "c++-compile"),
    ("c++", "c++-compile"),
    ("C", "c++-compile"),
    ("S", "preprocess-assemble"),
    ("s", "assemble"),
];

/// The extensions of header files.
const HEADER_EXTENSIONS: &[&str] = &["h", "hh", "hpp", "hxx", "h++", "inc", "inl", "ipp", "tcc", "H", "tlh", "tli"];

fn extension(artifact: &Artifact) -> &'static str {
    let name = artifact.exec_path.name().as_str();
    name.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("")
}

/// The union of the files of the targets an attribute names.
fn files_of<'v>(input: &RuleInput, prepared: &Prepared<'v>, attr: &str, heap: Heap<'v>) -> Value<'v> {
    let transitive: Vec<Value<'v>> = prepared
        .targets(input, attr)
        .into_iter()
        .filter_map(|target| TargetValue::from_value(target)?.default_info("files"))
        .collect();
    api::depset(Vec::new(), transitive, Order::Default, heap)
}

/// `cc_toolchain`: a `CcToolchainInfo` from its configuration and files.
pub fn cc_toolchain<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let config = prepared
        .targets(input, "toolchain_config")
        .first()
        .and_then(|target| TargetValue::from_value(*target)?.provider("CcToolchainConfigInfo"))
        .and_then(|config| config.downcast_ref::<ToolchainConfigValue>().map(|config| config.0.clone()))
        .ok_or_else(|| anyhow::anyhow!("toolchain_config must provide CcToolchainConfigInfo"))?;
    let mut files = SmallMap::new();
    for attr in TOOLCHAIN_FILES {
        files.insert(attr.to_string(), files_of(input, prepared, attr, heap));
    }
    let all_files = files.get("all_files").copied().unwrap_or_else(|| api::empty_depset(heap));
    let toolchain = heap.alloc(CcToolchain { config: config.clone(), files });
    let toolchain_info = providers::instance("ToolchainInfo", vec![("cc", toolchain), ("cc_provider_in_toolchain", Value::new_bool(true))], heap)?;
    let variables = make_variables(&config, heap);
    let template_variables = providers::instance("TemplateVariableInfo", vec![("variables", variables)], heap)?;
    let default_info = providers::instance("DefaultInfo", vec![("files", all_files)], heap)?;
    Ok(heap.alloc(AllocList([toolchain, toolchain_info, template_variables, default_info])))
}

/// The make variables a C++ toolchain gives genrules, such as `$(CC)`.
fn make_variables<'v>(config: &super::config::CcToolchainConfig, heap: Heap<'v>) -> Value<'v> {
    let tool = |name: &str| config.tool_path(name).unwrap_or_default().to_string();
    let mut variables = vec![
        ("CC".to_string(), tool("gcc")),
        ("AR".to_string(), tool("ar")),
        ("NM".to_string(), tool("nm")),
        ("LD".to_string(), tool("ld")),
        ("OBJCOPY".to_string(), tool("objcopy")),
        ("STRIP".to_string(), tool("strip")),
        ("GCOVTOOL".to_string(), tool("gcov")),
        ("C_COMPILER".to_string(), config.compiler.clone()),
        ("CC_FLAGS".to_string(), String::new()),
    ];
    variables.extend(config.make_variables.iter().cloned());
    heap.alloc(AllocDict(variables))
}

/// `cc_toolchain_alias`: the providers of the resolved C++ toolchain.
pub fn cc_toolchain_alias<'v>(input: &RuleInput, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let Some(target) = input.toolchains.resolved.iter().find_map(|(_, target)| target.as_ref()) else {
        return Ok(Value::new_none());
    };
    let value = heap.access_owned_frozen_value(&target.value);
    let Some(target) = TargetValue::from_value(value) else {
        return Ok(Value::new_none());
    };
    Ok(heap.alloc(AllocList(target.providers.values().copied())))
}

/// The resolved C++ toolchain of a rule, as `CcToolchainInfo`.
fn resolved_toolchain<'v>(input: &RuleInput, heap: Heap<'v>) -> anyhow::Result<&'v CcToolchain<'v>> {
    let wanted = Label::parse_canonical(CC_TOOLCHAIN_TYPE).ok();
    let target = input
        .toolchains
        .resolved
        .iter()
        .find(|(toolchain_type, _)| Some(*toolchain_type) == wanted)
        .and_then(|(_, target)| target.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no C++ toolchain was found for {}", input.label))?;
    let value = heap.access_owned_frozen_value(&target.value);
    let toolchain = TargetValue::from_value(value)
        .and_then(|target| target.provider("CcToolchainInfo"))
        .and_then(CcToolchain::from_value)
        .ok_or_else(|| anyhow::anyhow!("the C++ toolchain {} provides no CcToolchainInfo", target.label))?;
    Ok(toolchain)
}

/// The features a C++ rule enables: its `features` attribute and the
/// compilation mode's.
fn configure(input: &RuleInput, toolchain: &CcToolchain<'_>) -> anyhow::Result<Arc<FeatureConfiguration>> {
    let mut requested = vec![input.config.options.compilation_mode.clone()];
    let mut unsupported = Vec::new();
    if let Some(AttrValue::StringList(features)) = input.attr("features") {
        for feature in features {
            match feature.strip_prefix('-') {
                Some(disabled) => unsupported.push(disabled.to_string()),
                None => requested.push(feature.clone()),
            }
        }
    }
    Ok(Arc::new(toolchain.config.features.configure(&requested, &unsupported)?))
}

fn string_list(input: &RuleInput, attr: &str) -> Vec<String> {
    match input.attr(attr) {
        Some(AttrValue::StringList(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// The `CcInfo`s of the targets an attribute names.
fn cc_infos<'v>(input: &RuleInput, prepared: &Prepared<'v>, attr: &str) -> Vec<Value<'v>> {
    prepared
        .targets(input, attr)
        .into_iter()
        .filter_map(|target| TargetValue::from_value(target)?.provider("CcInfo"))
        .collect()
}

fn field<'v>(info: Value<'v>, context: &str, name: &str) -> Option<Value<'v>> {
    let context = CcStruct::field_of(info, "CcInfo", context)?;
    CcStruct::from_value(context)?.fields.get(name).copied()
}

/// What compiling a rule's sources needs from it and its dependencies.
struct Compilation<'v> {
    headers: Vec<Artifact>,
    /// Depsets of the dependencies' headers.
    dep_headers: Vec<Value<'v>>,
    includes: Vec<String>,
    quote_includes: Vec<String>,
    system_includes: Vec<String>,
    defines: Vec<String>,
    local_defines: Vec<String>,
    copts: Vec<String>,
}

impl<'v> Compilation<'v> {
    fn new(input: &RuleInput, state: &RuleState, prepared: &Prepared<'v>, srcs: &[Artifact]) -> anyhow::Result<Compilation<'v>> {
        let deps: Vec<Value<'v>> = ["deps", "implementation_deps", "malloc", "link_extra_lib"]
            .iter()
            .flat_map(|attr| cc_infos(input, prepared, attr))
            .collect();
        let mut headers = prepared.files(input, "hdrs")?;
        headers.extend(prepared.files(input, "textual_hdrs")?);
        headers.extend(srcs.iter().filter(|artifact| HEADER_EXTENSIONS.contains(&extension(artifact))).copied());
        let strings_of = |name: &str| -> Vec<String> {
            let mut out = Vec::new();
            for info in &deps {
                out.extend(api::items(field(*info, "compilation_context", name)).into_iter().map(crate::args::stringify));
            }
            out
        };
        let repo = artifact::repo_dir(input.label.package.repo).to_string();
        let bin = input.config.bin_dir();
        let repo_root = if repo.is_empty() { ".".to_string() } else { repo.clone() };
        let repo_bin = if repo.is_empty() { bin.clone() } else { format!("{bin}/{repo}") };
        let mut quote_includes = vec![repo_root, repo_bin];
        quote_includes.extend(strings_of("quote_includes"));
        let package = artifact::package_dir(input.label.package).to_string();
        let mut system_includes = Vec::new();
        for include in string_list(input, "includes") {
            let relative = join(&package, &include);
            system_includes.push(relative.clone());
            system_includes.push(join(&bin, &relative));
        }
        system_includes.extend(strings_of("system_includes"));
        let mut defines: Vec<String> = string_list(input, "defines").into_iter().map(|define| expand(state, &define)).collect::<anyhow::Result<_>>()?;
        defines.extend(strings_of("defines"));
        let local_defines = string_list(input, "local_defines").into_iter().map(|define| expand(state, &define)).collect::<anyhow::Result<_>>()?;
        let mut copts = config_copts(input);
        for copt in string_list(input, "copts") {
            copts.extend(shell_split(&expand(state, &copt)?));
        }
        Ok(Compilation {
            headers,
            dep_headers: deps.iter().filter_map(|info| field(*info, "compilation_context", "headers")).collect(),
            includes: strings_of("includes"),
            quote_includes: dedup(quote_includes),
            system_includes: dedup(system_includes),
            defines: dedup(defines),
            local_defines,
            copts,
        })
    }

    /// The rule's own `CompilationContext`, merged with its dependencies'.
    fn context(&self, input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> Value<'v> {
        let strings = |values: &[String]| api::depset(values.iter().map(|value| heap.alloc(value.as_str())).collect(), Vec::new(), Order::Default, heap);
        let own_headers: Vec<Value<'v>> = self.headers.iter().map(|artifact| heap.alloc(FileValue(*artifact))).collect();
        let headers = api::depset(own_headers.clone(), self.dep_headers.clone(), Order::Default, heap);
        let own = api::compilation_context(
            CompilationFields {
                headers,
                direct_public_headers: own_headers,
                direct_private_headers: Vec::new(),
                includes: strings(&self.includes),
                quote_includes: strings(&self.quote_includes),
                system_includes: strings(&self.system_includes),
                framework_includes: api::empty_depset(heap),
                defines: strings(&string_list(input, "defines")),
                local_defines: strings(&self.local_defines),
            },
            heap,
        );
        let mut contexts = vec![own];
        contexts.extend(cc_infos(input, prepared, "deps").into_iter().filter_map(|info| CcStruct::field_of(info, "CcInfo", "compilation_context")));
        let merged = api::merge_compilation_contexts(&contexts, heap);
        let Some(merged) = CcStruct::from_value(merged) else {
            return own;
        };
        let mut fields: Vec<(&str, Value<'v>)> = merged.fields.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        for (name, value) in &mut fields {
            if let Some(own_value) = CcStruct::field_of(own, "CompilationContext", name).filter(|_| name.starts_with("direct_")) {
                *value = own_value;
            }
        }
        heap.alloc(CcStruct::new("CompilationContext", fields))
    }

    /// Every header a compile action may read.
    fn all_headers(&self) -> Vec<Artifact> {
        let mut headers = self.headers.clone();
        for depset in &self.dep_headers {
            if let Some(depset) = Depset::from_value(*depset) {
                headers.extend(depset.to_list().into_iter().filter_map(FileValue::artifact));
            }
        }
        headers.sort();
        headers.dedup();
        headers
    }
}

fn join(base: &str, path: &str) -> String {
    let path = path.trim_end_matches('/');
    match (base.is_empty(), path.is_empty() || path == ".") {
        (true, _) => path.to_string(),
        (false, true) => base.to_string(),
        (false, false) => format!("{base}/{path}"),
    }
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values.into_iter().filter(|value| seen.insert(value.clone())).collect()
}

/// `--copt`, and so on, from the configuration.
fn config_copts(input: &RuleInput) -> Vec<String> {
    match input.config.value("copt") {
        Some(copts) => copts.split(' ').filter(|copt| !copt.is_empty()).map(str::to_string).collect(),
        None => Vec::new(),
    }
}

fn expand(state: &RuleState, text: &str) -> anyhow::Result<String> {
    state.expand(text, &[], crate::expand::LocationStyle::Exec)
}

/// Split an attribute value into words, as Bazel tokenizes `copts`.
fn shell_split(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (Some(open), c) if c == open => quote = None,
            (_, '\\') => {
                current.extend(chars.next());
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            (_, c) => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// The environment of C++ actions: the toolchain's, and the default shell
/// environment.
fn action_env(state: &RuleState, configuration: &FeatureConfiguration, action: &str, variables: &Variables) -> anyhow::Result<Vec<(String, String)>> {
    let mut env: std::collections::BTreeMap<String, String> = state.config.options.action_env.iter().cloned().collect();
    env.insert("PWD".to_string(), "/proc/self/cwd".to_string());
    env.extend(configuration.environment(action, variables)?);
    Ok(env.into_iter().collect())
}

/// Compile a rule's sources, returning the object files.
fn compile(
    state: &RuleState,
    toolchain: &CcToolchain<'_>,
    configuration: &FeatureConfiguration,
    compilation: &Compilation<'_>,
    srcs: &[Artifact],
) -> anyhow::Result<Vec<Artifact>> {
    let use_pic = configuration.is_enabled("supports_pic");
    let headers = compilation.all_headers();
    let toolchain_files = toolchain_files(toolchain, "compiler_files");
    let package = artifact::package_dir(state.label.package);
    let mut objects = Vec::new();
    for source in srcs {
        let Some((_, action)) = SOURCE_ACTIONS.iter().find(|(extension, _)| *extension == self::extension(source)) else {
            continue;
        };
        let relative = source.exec_path.strip_prefix(package).map(|path| path.to_string()).unwrap_or_else(|| source.exec_path.name().to_string());
        let stem = relative.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&relative);
        let suffix = if use_pic { ".pic" } else { "" };
        let object = state.declare(&format!("_objs/{}/{stem}{suffix}.o", state.label.name), false)?;
        let mut outputs = vec![object];
        let mut variables = api::toolchain_variables(&toolchain.config);
        variables.string("source_file", source.exec_path.to_string());
        variables.string("output_file", object.exec_path.to_string());
        if configuration.is_enabled("dependency_file") {
            let dependency = state.declare(&format!("_objs/{}/{stem}{suffix}.d", state.label.name), false)?;
            variables.string("dependency_file", dependency.exec_path.to_string());
            outputs.push(dependency);
        }
        if use_pic {
            variables.string("pic", "");
        }
        variables.strings("user_compile_flags", compilation.copts.clone());
        variables.strings("include_paths", compilation.includes.clone());
        variables.strings("quote_include_paths", compilation.quote_includes.clone());
        variables.strings("system_include_paths", compilation.system_includes.clone());
        variables.strings("preprocessor_defines", compilation.defines.iter().chain(&compilation.local_defines).cloned());
        let mut arguments = vec![configuration.tool(action)?.path.clone()];
        arguments.extend(configuration.command_line(action, &variables)?);
        let mut inputs = vec![*source];
        inputs.extend(headers.iter().copied());
        inputs.extend(toolchain_files.iter().copied());
        inputs.sort();
        inputs.dedup();
        state.register(Action {
            mnemonic: Symbol::intern("CppCompile"),
            progress_message: format!("Compiling {}", source.short_path()),
            inputs,
            runfiles: Vec::new(),
            outputs,
            kind: ActionKind::Spawn(Spawn {
                arguments,
                env: action_env(state, configuration, action, &variables)?,
                execution_info: Vec::new(),
            }),
        })?;
        objects.push(object);
    }
    Ok(objects)
}

/// The files a toolchain's tools need for some actions, or all of them.
fn toolchain_files(toolchain: &CcToolchain<'_>, set: &str) -> Vec<Artifact> {
    let files = toolchain
        .file_set(set)
        .and_then(Depset::from_value)
        .map(|depset| depset.to_list())
        .filter(|files| !files.is_empty())
        .or_else(|| toolchain.file_set("all_files").and_then(Depset::from_value).map(|depset| depset.to_list()))
        .unwrap_or_default();
    files.into_iter().filter_map(FileValue::artifact).collect()
}

/// Archive object files into `lib<name>.a`.
pub fn archive(
    state: &RuleState,
    toolchain: &CcToolchain<'_>,
    configuration: &FeatureConfiguration,
    name: &str,
    objects: &[Artifact],
    alwayslink: bool,
) -> anyhow::Result<Artifact> {
    let action = "c++-link-static-library";
    let extension = if alwayslink { "lo" } else { "a" };
    let (directory, base) = match name.rsplit_once('/') {
        Some((directory, base)) => (format!("{directory}/"), base),
        None => (String::new(), name),
    };
    let output = state.declare(&format!("{directory}lib{base}.{extension}"), false)?;
    let mut variables = api::toolchain_variables(&toolchain.config);
    variables.string("output_execpath", output.exec_path.to_string());
    let libraries = objects.iter().map(|object| api::library_variable("object_file", &object.exec_path.to_string(), &[], false)).collect();
    variables.set("libraries_to_link", super::features::Variable::Sequence(libraries));
    let mut arguments = vec![configuration.tool(action)?.path.clone()];
    arguments.extend(configuration.command_line(action, &variables)?);
    let mut inputs = objects.to_vec();
    inputs.extend(toolchain_files(toolchain, "ar_files"));
    state.register(Action {
        mnemonic: Symbol::intern("CppArchive"),
        progress_message: format!("Linking {}", output.short_path()),
        inputs,
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Spawn(Spawn {
            arguments,
            env: action_env(state, configuration, action, &variables)?,
            execution_info: Vec::new(),
        }),
    })?;
    Ok(output)
}

/// The `LinkerInput`s of `CcInfo`s, in link order.
fn linker_inputs<'v>(infos: &[Value<'v>], heap: Heap<'v>) -> Vec<Value<'v>> {
    let contexts: Vec<Value<'v>> = infos.iter().filter_map(|info| CcStruct::field_of(*info, "CcInfo", "linking_context")).collect();
    let merged = api::merge_linking_contexts(&contexts, heap);
    api::items(CcStruct::field_of(merged, "LinkingContext", "linker_inputs"))
}

/// What a link needs from the libraries it links: the
/// `libraries_to_link` variable, the files, and the flags.
struct Libraries {
    variables: Vec<super::features::Variable>,
    inputs: Vec<Artifact>,
    flags: Vec<String>,
}

fn libraries(objects: &[Artifact], inputs: &[Value<'_>]) -> Libraries {
    let mut libraries = Libraries {
        variables: objects.iter().map(|object| api::library_variable("object_file", &object.exec_path.to_string(), &[], false)).collect(),
        inputs: objects.to_vec(),
        flags: Vec::new(),
    };
    for input in inputs {
        let Some(input) = CcStruct::from_value(*input) else {
            continue;
        };
        for library in api::items(input.fields.get("libraries").copied()) {
            add_library(&mut libraries, library);
        }
        libraries.flags.extend(api::items(input.fields.get("user_link_flags").copied()).into_iter().map(crate::args::stringify));
        libraries.inputs.extend(api::items(input.fields.get("additional_inputs").copied()).into_iter().filter_map(FileValue::artifact));
    }
    libraries
}

/// Link a library statically if it has an archive, or its objects.
fn add_library(libraries: &mut Libraries, library: Value<'_>) {
    let file = |name: &str| CcStruct::field_of(library, "LibraryToLink", name).and_then(FileValue::artifact);
    let alwayslink = CcStruct::field_of(library, "LibraryToLink", "alwayslink").is_some_and(|value| value.to_bool());
    if let Some(archive) = file("static_library").or_else(|| file("pic_static_library")) {
        libraries.variables.push(api::library_variable("static_library", &archive.exec_path.to_string(), &[], alwayslink));
        libraries.inputs.push(archive);
        return;
    }
    let objects: Vec<Artifact> = ["objects", "pic_objects"]
        .iter()
        .flat_map(|name| api::items(CcStruct::field_of(library, "LibraryToLink", name)))
        .filter_map(FileValue::artifact)
        .collect();
    if !objects.is_empty() {
        let names: Vec<String> = objects.iter().map(|object| object.exec_path.to_string()).collect();
        libraries.variables.push(api::library_variable("object_file_group", "", &names, alwayslink));
        libraries.inputs.extend(objects);
        return;
    }
    if let Some(dynamic) = file("dynamic_library").or_else(|| file("interface_library")) {
        let name = dynamic.exec_path.name().as_str().trim_start_matches("lib");
        let name = name.split(".so").next().unwrap_or(name);
        libraries.variables.push(api::library_variable("dynamic_library", name, &[], false));
        libraries.inputs.push(dynamic);
    }
}

/// Link an executable.
fn link_executable(
    input: &RuleInput,
    state: &RuleState,
    toolchain: &CcToolchain<'_>,
    configuration: &FeatureConfiguration,
    libraries: Libraries,
    linkopts: Vec<String>,
) -> anyhow::Result<Artifact> {
    let action = "c++-link-executable";
    let output = state.declare(state.label.name.as_str(), false)?;
    let mut user_link_flags = linkopts;
    user_link_flags.extend(libraries.flags);
    let link = LinkVariables {
        library_search_directories: Vec::new(),
        runtime_library_search_directories: Vec::new(),
        user_link_flags,
        output: Some(output.exec_path.to_string()),
        param_file: None,
        is_using_linker: true,
        is_linking_dynamic_library: false,
        // `--strip=sometimes`, the default, strips fastbuild binaries.
        must_keep_debug: input.config.options.compilation_mode != "fastbuild",
        libraries_to_link: Some(libraries.variables),
    };
    let variables = link.variables(&toolchain.config);
    let mut arguments = vec![configuration.tool(action)?.path.clone()];
    arguments.extend(configuration.command_line(action, &variables)?);
    let mut inputs = libraries.inputs;
    inputs.extend(toolchain_files(toolchain, "linker_files"));
    inputs.sort();
    inputs.dedup();
    state.register(Action {
        mnemonic: Symbol::intern("CppLink"),
        progress_message: format!("Linking {}", output.short_path()),
        inputs,
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Spawn(Spawn {
            arguments,
            env: action_env(state, configuration, action, &variables)?,
            execution_info: Vec::new(),
        }),
    })?;
    Ok(output)
}

fn linkopts(input: &RuleInput, state: &RuleState) -> anyhow::Result<Vec<String>> {
    let mut flags = Vec::new();
    for linkopt in string_list(input, "linkopts") {
        flags.extend(shell_split(&expand(state, &linkopt)?));
    }
    if let Some(config) = input.config.value("linkopt") {
        flags.extend(config.split(' ').filter(|flag| !flag.is_empty()).map(str::to_string));
    }
    Ok(flags)
}

/// The runfiles of `data` and `deps`.
fn runfiles<'v>(input: &RuleInput, prepared: &Prepared<'v>, own: Vec<Artifact>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut all: Vec<&Runfiles<'v>> = Vec::new();
    let own = heap.alloc(Runfiles::new(providers::file_depset(&own, heap)));
    all.extend(Runfiles::from_value(own));
    for attr in ["data", "deps"] {
        for target in prepared.targets(input, attr) {
            let runfiles = TargetValue::from_value(target).and_then(|target| target.default_info("default_runfiles"));
            all.extend(runfiles.and_then(Runfiles::from_value));
        }
    }
    Ok(heap.alloc(Runfiles::merge_all(&all, heap)))
}

/// `cc_library`: compile, archive, and provide `CcInfo`.
pub fn cc_library<'v>(input: &RuleInput, prepared: &Prepared<'v>, state: &RuleState, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let toolchain = resolved_toolchain(input, heap)?;
    let configuration = configure(input, toolchain)?;
    let srcs = prepared.files(input, "srcs")?;
    let compilation = Compilation::new(input, state, prepared, &srcs)?;
    let objects = compile(state, toolchain, &configuration, &compilation, &srcs)?;
    let alwayslink = input.bool("alwayslink");
    let mut files = Vec::new();
    let mut library = LibraryFiles {
        alwayslink,
        ..Default::default()
    };
    if !objects.is_empty() {
        let archive = archive(state, toolchain, &configuration, input.label.name.as_str(), &objects, alwayslink)?;
        files.push(archive);
        let archive = heap.alloc(FileValue(archive));
        let objects: Vec<Value<'v>> = objects.iter().map(|object| heap.alloc(FileValue(*object))).collect();
        match configuration.is_enabled("supports_pic") {
            true => {
                library.pic_static_library = Some(archive);
                library.pic_objects = objects;
            }
            false => {
                library.static_library = Some(archive);
                library.objects = objects;
            }
        }
    }
    let owner = heap.alloc(LabelValue(input.label));
    let libraries = if objects.is_empty() { Vec::new() } else { vec![api::library_to_link(library, heap)] };
    let own_input = api::linker_input(owner, libraries, linkopts(input, state)?, Vec::new(), heap);
    let deps = cc_infos(input, prepared, "deps");
    let transitive: Vec<Value<'v>> = deps.iter().filter_map(|info| field(*info, "linking_context", "linker_inputs")).collect();
    let linking = api::linking_context(vec![own_input], transitive, heap);
    let compilation_context = compilation.context(input, prepared, heap);
    let cc_info = api::cc_info(compilation_context, linking, heap);
    let default_info = providers::instance(
        "DefaultInfo",
        vec![("files", providers::file_depset(&files, heap)), ("runfiles", runfiles(input, prepared, Vec::new(), heap)?)],
        heap,
    )?;
    Ok(heap.alloc(AllocList([cc_info, default_info])))
}

/// The outputs `cc_binary` always declares, built when asked for: the
/// stripped executable, and the debug info package, empty without fission.
fn implicit_outputs(state: &RuleState, toolchain: &CcToolchain<'_>, configuration: &FeatureConfiguration, prepared: &Prepared<'_>, executable: Artifact) -> anyhow::Result<()> {
    for (_, artifact) in &prepared.outputs.implicit {
        let name = artifact.exec_path.name().as_str();
        if name.ends_with(".stripped") {
            strip(state, toolchain, configuration, executable, *artifact)?;
        } else if name.ends_with(".dwp") {
            state.register(Action {
                mnemonic: Symbol::intern("CcGenerateDwp"),
                progress_message: String::new(),
                inputs: Vec::new(),
                runfiles: Vec::new(),
                outputs: vec![*artifact],
                kind: ActionKind::Write {
                    content: String::new(),
                    executable: false,
                },
            })?;
        }
    }
    Ok(())
}

/// Strip an executable's symbols, as Bazel's `strip` action does.
fn strip(state: &RuleState, toolchain: &CcToolchain<'_>, configuration: &FeatureConfiguration, input: Artifact, output: Artifact) -> anyhow::Result<()> {
    let action = "strip";
    let tool = match configuration.tool(action) {
        Ok(tool) => tool.path.clone(),
        Err(_) => toolchain.config.tool_path("strip").unwrap_or("strip").to_string(),
    };
    let mut variables = api::toolchain_variables(&toolchain.config);
    variables.string("output_file", output.exec_path.to_string());
    variables.string("input_file", input.exec_path.to_string());
    variables.strings("stripopts", Vec::new());
    let mut arguments = vec![tool, "-S".to_string(), "-p".to_string(), "-o".to_string(), output.exec_path.to_string(), input.exec_path.to_string()];
    arguments.extend(configuration.command_line(action, &variables)?);
    let mut inputs = vec![input];
    inputs.extend(toolchain_files(toolchain, "strip_files"));
    state.register(Action {
        mnemonic: Symbol::intern("CcStrip"),
        progress_message: format!("Stripping {}", output.short_path()),
        inputs,
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Spawn(Spawn {
            arguments,
            env: action_env(state, configuration, action, &variables)?,
            execution_info: Vec::new(),
        }),
    })
}

/// `cc_binary` and `cc_test`: compile, and link with their dependencies.
pub fn cc_binary<'v>(input: &RuleInput, prepared: &Prepared<'v>, state: &RuleState, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let toolchain = resolved_toolchain(input, heap)?;
    let configuration = configure(input, toolchain)?;
    let srcs = prepared.files(input, "srcs")?;
    let compilation = Compilation::new(input, state, prepared, &srcs)?;
    let objects = compile(state, toolchain, &configuration, &compilation, &srcs)?;
    let mut deps = cc_infos(input, prepared, "deps");
    // Binaries also link the allocator and the extra libraries the
    // configuration names.
    deps.extend(cc_infos(input, prepared, "malloc"));
    deps.extend(cc_infos(input, prepared, "link_extra_lib"));
    let inputs = linker_inputs(&deps, heap);
    let executable = link_executable(input, state, toolchain, &configuration, libraries(&objects, &inputs), linkopts(input, state)?)?;
    implicit_outputs(state, toolchain, &configuration, prepared, executable)?;
    let default_info = providers::instance(
        "DefaultInfo",
        vec![
            ("files", providers::file_depset(&[executable], heap)),
            ("executable", heap.alloc(FileValue(executable))),
            ("runfiles", runfiles(input, prepared, Vec::new(), heap)?),
        ],
        heap,
    )?;
    let cc_info = api::cc_info(compilation.context(input, prepared, heap), api::linking_context(Vec::new(), Vec::new(), heap), heap);
    Ok(heap.alloc(AllocList([default_info, cc_info])))
}
