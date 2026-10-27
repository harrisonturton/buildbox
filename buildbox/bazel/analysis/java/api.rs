//! `java_common_internal_do_not_use`: the functions `@_builtins`'
//! `java_common.internal_DO_NOT_USE()` gives rules_java.

use super::compile::{JavacExtras, JavacOutputs, Sources, StrictDeps, compile, compile_headers};
use super::toolchain::{BootClassPath, Plugins, Toolchain, field, files, strings};
use crate::action_value::target_kind;
use crate::args::Args;
use crate::artifact::Artifact;
use crate::expand::LocationStyle;
use crate::file_value::FileValue;
use crate::state::RuleState;
use crate::target_value::TargetValue;
use bazel_label::Label;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::provider::{Provider, ProviderInstance};
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::list::AllocList;
use starlark::values::none::NoneType;
use starlark::values::{Heap, Value, ValueLike};

/// The toolchain type a rule must declare to use `java_common`.
const JAVA_TOOLCHAIN_TYPE: &str = "@@bazel_tools//tools/jdk:toolchain_type";

#[starlark_module]
fn java_common_internal(builder: &mut GlobalsBuilder) {
    /// Register Turbine's header compilation of a target's sources.
    fn create_header_compilation_action<'v>(
        ctx: Value<'v>,
        java_toolchain: Value<'v>,
        compile_jar: Value<'v>,
        compile_deps_proto: Value<'v>,
        plugin_info: Value<'v>,
        source_files: Value<'v>,
        source_jars: Value<'v>,
        compilation_classpath: Value<'v>,
        direct_jars: Value<'v>,
        bootclasspath: Value<'v>,
        compile_time_java_deps: Value<'v>,
        javac_opts: Value<'v>,
        strict_deps_mode: &str,
        target_label: Value<'v>,
        injecting_rule_kind: Value<'v>,
        enable_direct_classpath: bool,
        additional_inputs: Value<'v>,
        header_compilation_jar: Value<'v>,
        header_compilation_direct_deps: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = ctx;
        let state = RuleState::get(eval)?;
        check_toolchain_declared(state)?;
        let heap = eval.heap();
        let sources = Sources {
            toolchain: Toolchain { info: java_toolchain, heap },
            plugins: Plugins::of(plugin_info, "api_generating_plugins", heap),
            source_files: unique(files(Some(source_files))),
            source_jars: files(Some(source_jars)),
            classpath: files(Some(compilation_classpath)),
            direct_jars: files(Some(direct_jars)),
            bootclasspath: bootclasspath_or_default(bootclasspath, java_toolchain, heap),
            dependency_artifacts: files(Some(compile_time_java_deps)),
            javac_opts: javac_opts_of(javac_opts),
            strict_deps: StrictDeps::parse(strict_deps_mode)?,
            target_label: label(target_label)?,
            injecting_rule_kind: string_or_none(injecting_rule_kind),
            additional_inputs: files(Some(additional_inputs)),
        };
        let output = artifact(compile_jar, "compile_jar")?;
        let deps = artifact(compile_deps_proto, "compile_deps_proto")?;
        let header_output = FileValue::artifact(header_compilation_jar);
        let header_direct_jars = files(Some(header_compilation_direct_deps));
        compile_headers(state, sources, output, header_output, deps, &header_direct_jars, enable_direct_classpath)?;
        Ok(NoneType)
    }

    /// Register JavaBuilder's compilation of a target's sources, and the
    /// actions adding resources and generated classes.
    fn create_compilation_action<'v>(
        ctx: Value<'v>,
        java_toolchain: Value<'v>,
        output: Value<'v>,
        manifest_proto: Value<'v>,
        plugin_info: Value<'v>,
        compilation_classpath: Value<'v>,
        direct_jars: Value<'v>,
        bootclasspath: Value<'v>,
        javabuilder_jvm_flags: Value<'v>,
        compile_time_java_deps: Value<'v>,
        javac_opts: Value<'v>,
        strict_deps_mode: &str,
        target_label: Value<'v>,
        #[starlark(default = NoneType)] deps_proto: Value<'v>,
        #[starlark(default = NoneType)] gen_class: Value<'v>,
        #[starlark(default = NoneType)] gen_source: Value<'v>,
        #[starlark(default = NoneType)] native_header_jar: Value<'v>,
        #[starlark(default = NoneType)] sources: Value<'v>,
        #[starlark(default = NoneType)] source_jars: Value<'v>,
        #[starlark(default = NoneType)] resources: Value<'v>,
        #[starlark(default = NoneType)] resource_jars: Value<'v>,
        #[starlark(default = NoneType)] classpath_resources: Value<'v>,
        #[starlark(default = NoneType)] sourcepath: Value<'v>,
        #[starlark(default = NoneType)] injecting_rule_kind: Value<'v>,
        #[starlark(default = true)] enable_jspecify: bool,
        #[starlark(default = true)] enable_direct_classpath: bool,
        #[starlark(default = NoneType)] additional_inputs: Value<'v>,
        #[starlark(default = NoneType)] additional_outputs: Value<'v>,
        #[starlark(default = NoneType)] extra_args: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (enable_jspecify, enable_direct_classpath);
        let state = RuleState::get(eval)?;
        check_toolchain_declared(state)?;
        let heap = eval.heap();
        let compiled = Sources {
            toolchain: Toolchain { info: java_toolchain, heap },
            plugins: Plugins::of(plugin_info, "plugins", heap),
            source_files: unique(files(Some(sources))),
            source_jars: files(Some(source_jars)),
            classpath: files(Some(compilation_classpath)),
            direct_jars: files(Some(direct_jars)),
            bootclasspath: bootclasspath_or_default(bootclasspath, java_toolchain, heap),
            dependency_artifacts: files(Some(compile_time_java_deps)),
            javac_opts: javac_opts_of(javac_opts),
            strict_deps: StrictDeps::parse(strict_deps_mode)?,
            target_label: label(target_label)?,
            injecting_rule_kind: string_or_none(injecting_rule_kind),
            additional_inputs: files(Some(additional_inputs)),
        };
        let outputs = JavacOutputs {
            output: artifact(output, "output")?,
            manifest_proto: artifact(manifest_proto, "manifest_proto")?,
            deps_proto: FileValue::artifact(deps_proto),
            gen_class: FileValue::artifact(gen_class),
            gen_source: FileValue::artifact(gen_source),
            native_header: FileValue::artifact(native_header_jar),
            additional: files(Some(additional_outputs)),
        };
        let resources = files(Some(resources));
        let strip_prefix = resource_strip_prefix(ctx, heap);
        let mut placed = Vec::with_capacity(resources.len());
        for resource in resources {
            placed.push((java_resource_path(state, resource, strip_prefix.as_deref())?, resource));
        }
        let extras = JavacExtras {
            resources: placed,
            resource_jars: files(Some(resource_jars)),
            classpath_resources: files(Some(classpath_resources)),
            sourcepath: files(Some(sourcepath)),
            javabuilder_jvm_flags: strings(Some(javabuilder_jvm_flags)),
            extra_args: extra_arguments(extra_args)?,
        };
        compile(state, compiled, outputs, extras)?;
        Ok(NoneType)
    }

    /// The kind of rule of a dependency, such as `java_library`.
    fn target_kind<'v>(#[starlark(require = pos)] target: Value<'v>) -> anyhow::Result<String> {
        let Some(value) = TargetValue::from_value(target) else {
            return Ok(String::new());
        };
        let kind = target_kind(value as *const TargetValue as usize);
        Ok(kind.map(|kind| kind.to_string()).unwrap_or_default())
    }

    /// The directories of the runfiles tree that hold the shared libraries
    /// of `libraries`, a depset of `LibraryToLink`s.
    fn collect_native_deps_dirs<'v>(libraries: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let mut dirs: Vec<String> = Vec::new();
        for library in Depset::from_value(libraries).map(|depset| depset.to_list()).unwrap_or_default() {
            let file = field(library, "interface_library", heap).or_else(|| field(library, "dynamic_library", heap));
            let Some(artifact) = file.and_then(FileValue::artifact) else {
                continue;
            };
            let name = artifact.exec_path.name().to_string();
            if [".ifso", ".tbd", ".lib", ".dll.a"].iter().any(|extension| name.ends_with(extension)) {
                continue;
            }
            if !is_shared_library(&name) {
                anyhow::bail!("not a shared library :{}", artifact.exec_path);
            }
            let short_path = artifact.short_path();
            let dir = short_path.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        Ok(heap.alloc(AllocList(dirs)))
    }

    /// `jars` without `excluded_jars`, for an archive's classpath.
    fn get_runtime_classpath_for_archive<'v>(jars: Value<'v>, excluded_jars: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let excluded = files(Some(excluded_jars));
        if excluded.is_empty() {
            return Ok(jars);
        }
        let kept = files(Some(jars)).into_iter().filter(|jar| !excluded.contains(jar));
        Ok(heap.alloc(Depset {
            direct: kept.map(|jar| heap.alloc(FileValue(jar))).collect(),
            transitive: Vec::new(),
            order: Order::Default,
        }))
    }

    /// Fail unless each of `providers` is an instance of `provider_type`.
    fn check_provider_instances<'v>(providers: Value<'v>, what: &str, provider_type: Value<'v>) -> anyhow::Result<NoneType> {
        let Some(provider) = Provider::from_value(provider_type) else {
            anyhow::bail!("wanted Provider, got {}", provider_type.get_type());
        };
        let items = bazel_starlark::convert::items(providers).unwrap_or_default();
        for (index, item) in items.into_iter().enumerate() {
            let instance = ProviderInstance::from_value(item);
            if instance.and_then(|instance| instance.provider_key()) == Some(provider.key()) {
                continue;
            }
            let got = match instance {
                Some(instance) => instance.provider_name(),
                None => item.get_type().to_string(),
            };
            anyhow::bail!("at index {index} of {what}, got element of type {got}, want {}", provider.name());
        }
        Ok(NoneType)
    }

    fn _google_legacy_api_enabled() -> anyhow::Result<bool> {
        Ok(false)
    }

    fn _incompatible_java_info_merge_runtime_module_flags() -> anyhow::Result<bool> {
        Ok(false)
    }

    fn incompatible_disable_non_executable_java_binary() -> anyhow::Result<bool> {
        Ok(false)
    }

    /// Fail unless the rule declares the Java toolchain type.
    fn _check_java_toolchain_is_declared_on_rule<'v>(actions: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<NoneType> {
        let _ = actions;
        check_toolchain_declared(RuleState::get(eval)?)?;
        Ok(NoneType)
    }

    /// The values of a rule's list attribute with `$(location)` and make
    /// variables expanded, as exec paths or runfiles paths, split into
    /// words if `tokenize`.
    fn expand_java_opts<'v>(
        ctx: Value<'v>,
        attr: &str,
        #[starlark(require = named)] tokenize: bool,
        #[starlark(require = named, default = false)] exec_paths: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let state = RuleState::get(eval)?;
        let heap = eval.heap();
        let attrs = field(ctx, "attr", heap).ok_or_else(|| anyhow::anyhow!("expand_java_opts: ctx has no attr"))?;
        let values = strings(field(attrs, attr, heap));
        let style = if exec_paths { LocationStyle::Exec } else { LocationStyle::Root };
        let mut expanded = Vec::with_capacity(values.len());
        for value in values {
            let value = state.expand(&value, &[], style).map_err(|error| anyhow::anyhow!("in {attr} attribute: {error}"))?;
            match tokenize {
                true => expanded.extend(crate::shell::tokenize(&value).map_err(|error| anyhow::anyhow!("in {attr} attribute: {error}"))?),
                false => expanded.push(value),
            }
        }
        Ok(heap.alloc(AllocList(expanded)))
    }

    /// `--javacopt`s split into words, as Bazel tokenizes them.
    fn tokenize_javacopts<'v>(opts: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(AllocList(tokenize_java_options(strings(Some(opts))))))
    }
}

/// Javac options as Bazel's Java compilation takes them from their
/// depset: in reverse order, each split into words, or kept whole if it
/// can't be.
fn javac_opts_of(opts: Value<'_>) -> Vec<String> {
    let mut opts = strings(Some(opts));
    opts.reverse();
    tokenize_java_options(opts)
}

/// Options split into words, or kept whole when they don't split.
fn tokenize_java_options(opts: Vec<String>) -> Vec<String> {
    let mut words = Vec::new();
    for opt in opts {
        match crate::shell::tokenize(&opt) {
            Ok(split) => words.extend(split),
            Err(_) => words.push(opt),
        }
    }
    words
}

/// The rule's bootclasspath, if it gives a non-empty one, or the
/// toolchain's.
fn bootclasspath_or_default<'v>(given: Value<'v>, toolchain: Value<'v>, heap: Heap<'v>) -> BootClassPath {
    if ProviderInstance::from_value(given).is_some() {
        let bootclasspath = BootClassPath::of(given, heap);
        if !bootclasspath.is_empty() {
            return bootclasspath;
        }
    }
    Toolchain { info: toolchain, heap }.bootclasspath()
}

/// Files once each, in their first order, as Bazel's sets keep them.
fn unique(files: Vec<Artifact>) -> Vec<Artifact> {
    let mut seen = std::collections::HashSet::new();
    files.into_iter().filter(|file| seen.insert(*file)).collect()
}

fn artifact(value: Value<'_>, what: &str) -> anyhow::Result<Artifact> {
    FileValue::artifact(value).ok_or_else(|| anyhow::anyhow!("{what} must be a File, not {}", value.get_type()))
}

fn label(value: Value<'_>) -> anyhow::Result<Label> {
    LabelValue::from_value(value).map(|label| label.0).ok_or_else(|| anyhow::anyhow!("target_label must be a Label, not {}", value.get_type()))
}

fn string_or_none(value: Value<'_>) -> Option<String> {
    value.unpack_str().map(str::to_string)
}

/// The arguments of each `Args` of `extra_args`.
fn extra_arguments(value: Value<'_>) -> anyhow::Result<Vec<Vec<String>>> {
    if value.is_none() {
        return Ok(Vec::new());
    }
    let items = bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("extra_args must be a list of Args"))?;
    let mut arguments = Vec::with_capacity(items.len());
    for item in items {
        let args = item.downcast_ref::<Args>().ok_or_else(|| anyhow::anyhow!("extra_args must be a list of Args, not {}", item.get_type()))?;
        arguments.push(args.state().arguments);
    }
    Ok(arguments)
}

/// Fail unless the rule declares the Java toolchain type, as Bazel requires
/// of rules that use `java_common`.
fn check_toolchain_declared(state: &RuleState) -> anyhow::Result<()> {
    let java = Label::parse_canonical(JAVA_TOOLCHAIN_TYPE).expect("a valid label");
    if state.toolchain_types.contains(&java) {
        return Ok(());
    }
    anyhow::bail!(
        "Rule '{}' in '{}' must declare '{}' toolchain in order to use java_common. See https://github.com/bazelbuild/bazel/issues/18970.",
        state.kind,
        state.label,
        java
    )
}

/// The `resource_strip_prefix` the rule gives, if it gives one.
fn resource_strip_prefix<'v>(ctx: Value<'v>, heap: Heap<'v>) -> Option<String> {
    let attrs = field(ctx, "attr", heap)?;
    let prefix = field(attrs, "resource_strip_prefix", heap)?;
    prefix.unpack_str().filter(|prefix| !prefix.is_empty()).map(str::to_string)
}

/// A resource's path in the jar, as Bazel's `JavaHelper` places it:
/// relative to the rule's repository, then without `strip_prefix`, or
/// below its Java source root.
fn java_resource_path(state: &RuleState, resource: Artifact, strip_prefix: Option<&str>) -> anyhow::Result<String> {
    let mut path = resource.output_dir_relative().to_string();
    let repo = state.label.repo();
    if !repo.is_main() {
        let repo_dir = crate::artifact::repo_dir(repo).to_string();
        if let Some(relative) = path.strip_prefix(&format!("{repo_dir}/")) {
            path = relative.to_string();
        }
    }
    let Some(prefix) = strip_prefix else {
        return Ok(default_resource_path(&path));
    };
    match path.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('/')) {
        Some(stripped) => Ok(stripped.to_string()),
        None => anyhow::bail!("in resource_strip_prefix attribute: Resource file '{path}' is not under the specified prefix to strip"),
    }
}

/// A resource's path in the jar by default: below `src/*/resources`, or
/// below the Java source root, as Bazel's `JavaUtil` finds it.
fn default_resource_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    for index in 0..segments.len().saturating_sub(2) {
        if segments[index] == "src" && segments[index + 2] == "resources" {
            return segments[index + 3..].join("/");
        }
    }
    match java_root_index(&segments) {
        Some(index) => segments[index + 1..].join("/"),
        None => path.to_string(),
    }
}

/// The index of the segment that is a path's Java source root, as Bazel's
/// `JavaUtil.javaSegmentIndex` finds it.
fn java_root_index(segments: &[&str]) -> Option<usize> {
    const ROOTS: [&str; 4] = ["java", "javatests", "src", "testsrc"];
    let mut root = segments.iter().position(|segment| ROOTS.contains(segment))?;
    let is_src = segments[root] == "src";
    let mut maven = is_src.then_some(root);
    if root == 0 || is_src {
        let last = segments.len().saturating_sub(2);
        for index in root + 1..=last {
            let segment = segments[index];
            if segment == "src" || (is_src && (segment == "javatests" || segment == "java")) {
                let next = segments[index + 1];
                if ["com", "org", "net"].contains(&next) {
                    root = index;
                } else if segment == "src" {
                    maven = Some(index);
                }
                break;
            }
        }
    }
    if let Some(maven) = maven.filter(|maven| maven + 2 < segments.len()) {
        if ["main", "test"].contains(&segments[maven + 1]) && ["java", "resources"].contains(&segments[maven + 2]) {
            root = maven + 2;
        }
    }
    Some(root)
}

/// Whether a file is a shared library, as Bazel's C++ file types say.
fn is_shared_library(name: &str) -> bool {
    if [".so", ".dylib", ".dll", ".pyd", ".wasm"].iter().any(|extension| name.ends_with(extension)) {
        return true;
    }
    let Some((_, version)) = name.split_once(".so.") else {
        return false;
    };
    version.split('.').all(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()) && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Add `java_common_internal_do_not_use` to `_builtins.internal`.
pub fn register_builtins_internal(builder: &mut GlobalsBuilder) {
    builder.namespace("java_common_internal_do_not_use", java_common_internal);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_are_placed_below_java_roots() {
        assert_eq!(default_resource_path("java/com/ex/greeting.txt"), "com/ex/greeting.txt");
        assert_eq!(default_resource_path("app/src/main/resources/a.txt"), "a.txt");
        assert_eq!(default_resource_path("x/src/main/java/com/A.txt"), "com/A.txt");
        assert_eq!(default_resource_path("data/a.txt"), "data/a.txt");
    }

    #[test]
    fn shared_libraries_are_recognized() {
        assert!(is_shared_library("libfoo.so"));
        assert!(is_shared_library("libfoo.so.1.2"));
        assert!(!is_shared_library("libfoo.a"));
    }
}
