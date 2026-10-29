//! The actions that compile Java, as Bazel's `JavaCompilationHelper`
//! registers them: JavaBuilder's `Javac`, Turbine's header compilation,
//! singlejar's resource jar and GenClass's jar of generated classes.

use super::toolchain::{BootClassPath, Plugins, Runtime, Tool, Toolchain};
use crate::action::{Action, ActionKind, ParamFile, Spawn, environment};
use crate::artifact::Artifact;
use crate::inputs::InputSet;
use crate::state::{RuleState, runfiles_tree};
use bazel_label::Label;
use intern::Symbol;

/// How strictly Java compilation checks that code uses only its direct
/// dependencies, as `--strict_java_deps` sets it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StrictDeps {
    Off,
    Warn,
    Error,
}

impl StrictDeps {
    /// The mode a rule gives, as Bazel reads it: `DEFAULT` is `ERROR`.
    pub fn parse(mode: &str) -> anyhow::Result<StrictDeps> {
        match mode.to_ascii_uppercase().as_str() {
            "OFF" => Ok(StrictDeps::Off),
            "ERROR" | "DEFAULT" => Ok(StrictDeps::Error),
            "WARN" => Ok(StrictDeps::Warn),
            _ => anyhow::bail!("StrictDepsMode {mode} not allowed. Only OFF and ERROR values are accepted."),
        }
    }

    fn name(self) -> &'static str {
        match self {
            StrictDeps::Off => "OFF",
            StrictDeps::Warn => "WARN",
            StrictDeps::Error => "ERROR",
        }
    }
}

/// What a compilation of Java sources, or of their headers, takes.
pub struct Sources<'v> {
    pub toolchain: Toolchain<'v>,
    pub plugins: Plugins,
    pub source_files: Vec<Artifact>,
    pub source_jars: Vec<Artifact>,
    /// The compile-time classpath, direct jars first.
    pub classpath: Vec<Artifact>,
    pub direct_jars: Vec<Artifact>,
    /// The bootclasspath the rule gives, or the toolchain's.
    pub bootclasspath: BootClassPath,
    /// The `.jdeps` files of the dependencies.
    pub dependency_artifacts: Vec<Artifact>,
    pub javac_opts: Vec<String>,
    pub strict_deps: StrictDeps,
    pub target_label: Label,
    pub injecting_rule_kind: Option<String>,
    pub additional_inputs: Vec<Artifact>,
}

/// What `Javac` writes besides its class jar.
pub struct JavacOutputs {
    pub output: Artifact,
    pub manifest_proto: Artifact,
    pub deps_proto: Option<Artifact>,
    pub gen_class: Option<Artifact>,
    pub gen_source: Option<Artifact>,
    pub native_header: Option<Artifact>,
    pub additional: Vec<Artifact>,
}

/// What only full compilations take: resources, and JavaBuilder's flags.
pub struct JavacExtras {
    /// Resources at their paths in the jar.
    pub resources: Vec<(String, Artifact)>,
    pub resource_jars: Vec<Artifact>,
    pub classpath_resources: Vec<Artifact>,
    pub sourcepath: Vec<Artifact>,
    pub javabuilder_jvm_flags: Vec<String>,
    /// The arguments of `extra_args`, each list in a params file of its own.
    pub extra_args: Vec<Vec<String>>,
}

/// Register the actions that compile the sources into `outputs.output`:
/// `Javac`, and the actions that add resources and generated classes.
pub fn compile(state: &RuleState, sources: Sources<'_>, mut outputs: JavacOutputs, extras: JavacExtras) -> anyhow::Result<()> {
    check_supported(state, &sources.toolchain)?;
    let runtime = sources.toolchain.runtime()?;
    if let Some(gen_class) = outputs.gen_class {
        gen_class_action(state, &sources.toolchain, &runtime, outputs.output, outputs.manifest_proto, gen_class)?;
    }
    if !extras.resources.is_empty() || !extras.resource_jars.is_empty() || !extras.classpath_resources.is_empty() {
        let jar = outputs.output;
        outputs.output = state.declare_at_root(&append_without_extension(&jar.output_dir_relative().to_string(), "-class"))?;
        let mut resource_jars = extras.resource_jars.clone();
        resource_jars.push(outputs.output);
        resource_jar_action(state, &sources.toolchain, jar, &extras, &resource_jars, &sources.additional_inputs)?;
    }
    javac_action(state, sources, &runtime, &outputs, &extras)
}

/// Fail on what Bazel's Java compilation can do that bb doesn't support,
/// rather than compiling without it.
fn check_supported(state: &RuleState, toolchain: &Toolchain<'_>) -> anyhow::Result<()> {
    let options = &state.config.options;
    if options.bool("experimental_enable_jspecify") && toolchain.field("_jspecify_info").is_some() {
        anyhow::bail!("Java toolchains with JSpecify aren't supported by bb yet");
    }
    if options.bool("collect_code_coverage") {
        anyhow::bail!("coverage of Java isn't supported by bb yet");
    }
    if options.bool("experimental_local_java_optimizations") {
        anyhow::bail!("--experimental_local_java_optimizations isn't supported by bb yet");
    }
    if toolchain.field("_package_configuration").is_some_and(|configuration| configuration.length().unwrap_or(0) > 0) {
        anyhow::bail!("java_package_configuration isn't supported by bb yet");
    }
    if !options.strings("modify_execution_info").is_empty() {
        anyhow::bail!("--modify_execution_info isn't supported by bb yet");
    }
    Ok(())
}

/// JavaBuilder's `Javac` action, as Bazel's `JavaCompileActionBuilder`
/// makes it. Bazel runs it with a classpath reduced to what the
/// dependencies' `.jdeps` say is used, falling back to the full one; bb
/// runs it with the full classpath, which compiles the same.
fn javac_action(state: &RuleState, sources: Sources<'_>, runtime: &Runtime, outputs: &JavacOutputs, extras: &JavacExtras) -> anyhow::Result<()> {
    let toolchain = &sources.toolchain;
    let mut javabuilder = toolchain.tool("_javabuilder")?.ok_or_else(|| anyhow::anyhow!("the Java toolchain has no JavaBuilder"))?;
    javabuilder.jvm_opts.extend(extras.javabuilder_jvm_flags.iter().cloned());
    let (classpath, direct_jars) = with_auxiliary(&sources);
    let direct_jars = match sources.strict_deps {
        StrictDeps::Off => Vec::new(),
        _ => direct_jars,
    };
    let mode = classpath_mode(state, toolchain, &sources, outputs.deps_proto.is_some());
    let dependency_artifacts = match (sources.strict_deps, mode) {
        (StrictDeps::Off, _) | (_, ClasspathMode::Off) => Vec::new(),
        _ => sources.dependency_artifacts.clone(),
    };

    let mut tools = javabuilder.inputs(runtime);
    tools.extend(toolchain.files("tools"));
    let mut inputs = Vec::new();
    inputs.extend(sources.plugins.processor_jars.iter().copied());
    inputs.extend(sources.plugins.processor_data.iter().copied());
    inputs.extend(sources.source_jars.iter().copied());
    inputs.extend(sources.source_files.iter().copied());
    inputs.extend(runtime.files.iter().copied());
    inputs.extend(sources.bootclasspath.bootclasspath.iter().copied());
    inputs.extend(extras.sourcepath.iter().copied());
    inputs.extend(sources.additional_inputs.iter().copied());
    inputs.extend(sources.bootclasspath.system_inputs.iter().copied());
    inputs.extend(tools);
    inputs.extend(classpath.iter().copied());
    inputs.extend(dependency_artifacts.iter().copied());

    let mut flags = Line::default();
    flags.add_exec_path("--output", Some(outputs.output));
    flags.add_exec_path("--native_header_output", outputs.native_header);
    flags.add_exec_path("--generated_sources_output", outputs.gen_source);
    flags.add_exec_path("--output_manifest_proto", Some(outputs.manifest_proto));
    flags.add("--compress_jar");
    flags.add_exec_path("--output_deps_proto", outputs.deps_proto);
    flags.add_exec_paths("--bootclasspath", &sources.bootclasspath.bootclasspath);
    if let Some(system) = &sources.bootclasspath.system_path {
        flags.add("--system");
        flags.add(system);
    }
    flags.add_exec_paths("--sourcepath", &extras.sourcepath);
    flags.add_exec_paths("--processorpath", &sources.plugins.processor_jars);
    flags.add_all("--processors", &sources.plugins.processor_classes);
    flags.add_exec_paths("--source_jars", &sources.source_jars);
    flags.add_exec_paths("--sources", &sources.source_files);
    flags.add_javac_opts(&sources.javac_opts, false);
    flags.add_target_label(state, sources.target_label);
    flags.add_optional("--injecting_rule_kind", sources.injecting_rule_kind.as_deref());
    if sources.strict_deps != StrictDeps::Off {
        flags.add("--strict_java_deps");
        flags.add(sources.strict_deps.name());
        flags.add_exec_paths("--direct_dependencies", &direct_jars);
    }
    flags.add("--experimental_fix_deps_tool");
    flags.add(state.config.options.string("experimental_fix_deps_tool"));

    let mut classpath_line = Line::default();
    classpath_line.add_exec_paths("--classpath", &classpath);
    if mode == ClasspathMode::JavaBuilder {
        classpath_line.add("--reduce_classpath_mode");
        classpath_line.add("JAVABUILDER_REDUCED");
        classpath_line.add_exec_paths("--deps_artifacts", &dependency_artifacts);
    }
    let mut lines = vec![(flags.0, Params::Always), (classpath_line.0, Params::Always)];
    lines.extend(extras.extra_args.iter().map(|args| (args.clone(), Params::Always)));
    let command = command(state, outputs.output, javabuilder.command(toolchain, runtime), lines)?;

    let mut execution_info = vec![("supports-path-mapping".to_string(), "1".to_string())];
    for (field, requirement) in WORKER_REQUIREMENTS {
        if toolchain.bool(field) {
            execution_info.push((requirement.to_string(), "1".to_string()));
        }
    }
    let mut action_outputs = vec![outputs.output];
    action_outputs.extend(outputs.additional.iter().copied());
    action_outputs.extend([outputs.deps_proto, outputs.native_header, outputs.gen_source, Some(outputs.manifest_proto)].into_iter().flatten());
    let message = progress_message("Building", outputs.output, &sources);
    register_spawn(state, "Javac", message, inputs, action_outputs, command, execution_info, outputs.deps_proto)
}

/// The toolchain fields that say JavaBuilder runs as a worker, and the
/// execution requirements Bazel gives `Javac` for them.
const WORKER_REQUIREMENTS: [(&str, &str); 4] = [
    ("_javac_supports_workers", "supports-workers"),
    ("_javac_supports_multiplex_workers", "supports-multiplex-workers"),
    ("_javac_supports_worker_cancellation", "supports-worker-cancellation"),
    ("_javac_supports_worker_multiplex_sandboxing", "supports-multiplex-sandboxing"),
];

/// How JavaBuilder reduces the classpath, as `--experimental_java_classpath`
/// asks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClasspathMode {
    Off,
    JavaBuilder,
    Bazel,
}

/// The classpath mode of a compilation: off for strict deps off, without a
/// `.jdeps` output, or with processors that need the full classpath.
fn classpath_mode(state: &RuleState, toolchain: &Toolchain<'_>, sources: &Sources<'_>, has_deps_proto: bool) -> ClasspathMode {
    let incompatible = toolchain.strings("_reduced_classpath_incompatible_processors");
    let needs_full = sources.plugins.processor_classes.iter().any(|class| incompatible.contains(class));
    if needs_full || sources.strict_deps == StrictDeps::Off || !has_deps_proto {
        return ClasspathMode::Off;
    }
    match state.config.options.string("experimental_java_classpath") {
        "off" => ClasspathMode::Off,
        "javabuilder" => ClasspathMode::JavaBuilder,
        _ => ClasspathMode::Bazel,
    }
}

/// The classpath and direct jars, with the bootclasspath's auxiliary jars
/// first, as `Javac` gets them.
fn with_auxiliary(sources: &Sources<'_>) -> (Vec<Artifact>, Vec<Artifact>) {
    let auxiliary = &sources.bootclasspath.auxiliary;
    if auxiliary.is_empty() {
        return (sources.classpath.clone(), sources.direct_jars.clone());
    }
    let classpath = auxiliary.iter().chain(&sources.classpath).copied().collect();
    let direct_jars = auxiliary.iter().chain(&sources.direct_jars).copied().collect();
    (classpath, direct_jars)
}

/// Register Turbine's action, which compiles the sources' headers into
/// `output`, as Bazel's `JavaHeaderCompileAction` does: with only the
/// direct dependencies on the classpath unless annotation processors run.
pub fn compile_headers(
    state: &RuleState,
    sources: Sources<'_>,
    output: Artifact,
    header_compilation_output: Option<Artifact>,
    deps_output: Artifact,
    header_direct_jars: &[Artifact],
    enable_direct_classpath: bool,
) -> anyhow::Result<()> {
    check_supported(state, &sources.toolchain)?;
    let toolchain = &sources.toolchain;
    let runtime = toolchain.runtime()?;
    let processors = &sources.plugins.processor_classes;
    let use_direct_classpath = enable_direct_classpath && processors.is_empty();
    let builtin = toolchain.strings("_header_compiler_builtin_processors");
    let direct_compiler = toolchain.tool("_header_compiler_direct")?.filter(|_| processors.iter().all(|class| builtin.contains(class)));
    let use_direct_compiler = direct_compiler.is_some();
    let compiler: Tool<'_> = match direct_compiler {
        Some(compiler) => compiler,
        None => toolchain.tool("_header_compiler")?.ok_or_else(|| anyhow::anyhow!("the Java toolchain has no header compiler"))?,
    };
    let direct_jars = match sources.strict_deps {
        StrictDeps::Off => Vec::new(),
        _ => sources.direct_jars.clone(),
    };

    let mut inputs = sources.additional_inputs.clone();
    inputs.extend(sources.bootclasspath.bootclasspath.iter().copied());
    inputs.extend(sources.source_jars.iter().copied());
    inputs.extend(sources.source_files.iter().copied());
    let tools_jars = toolchain.files("tools");
    inputs.extend(tools_jars.iter().copied());
    inputs.extend(compiler.inputs(&runtime));

    let mut flags = Line::default();
    flags.add_exec_path("--output", Some(output));
    flags.add_exec_path("--header_compilation_output", header_compilation_output);
    flags.add_exec_path("--output_deps", Some(deps_output));
    flags.add_exec_paths("--bootclasspath", &sources.bootclasspath.bootclasspath);
    flags.add_exec_paths("--sources", &sources.source_files);
    flags.add_exec_paths("--source_jars", &sources.source_jars);
    flags.add_optional("--injecting_rule_kind", sources.injecting_rule_kind.as_deref());
    let hjar_opt = processors.iter().any(|class| class == "dagger.internal.codegen.ComponentProcessor");
    flags.add_javac_opts(&sources.javac_opts, hjar_opt);
    flags.add_target_label(state, sources.target_label);

    let mut outputs = vec![output, deps_output];
    outputs.extend(header_compilation_output);
    let message = progress_message("Compiling Java headers", output, &sources);
    let base_info = vec![("supports-path-mapping".to_string(), "1".to_string())];
    if use_direct_classpath {
        let classpath = match header_direct_jars.is_empty() && !sources.classpath.is_empty() {
            true => sources.classpath.clone(),
            false => header_direct_jars.to_vec(),
        };
        inputs.extend(classpath.iter().copied());
        flags.add_exec_paths("--classpath", &classpath);
        flags.add("--reduce_classpath_mode");
        flags.add("NONE");
        let command = command(state, output, compiler.command(toolchain, &runtime), vec![(flags.0, Params::WhenLong("multiline"))])?;
        return register_spawn(state, "Turbine", message, inputs, outputs, command, base_info, Some(deps_output));
    }

    if !use_direct_compiler {
        inputs.extend(sources.plugins.processor_jars.iter().copied());
        inputs.extend(sources.plugins.processor_data.iter().copied());
    }
    let builtin_processors: Vec<String> = processors.iter().filter(|class| builtin.contains(class)).cloned().collect();
    flags.add_all("--builtin_processors", &builtin_processors);
    flags.add_all("--processors", processors);
    if !use_direct_compiler {
        flags.add_exec_paths("--processorpath", &sources.plugins.processor_jars);
    }
    if sources.strict_deps != StrictDeps::Off {
        flags.add_exec_paths("--direct_dependencies", &direct_jars);
    }
    let mode = classpath_mode(state, toolchain, &sources, true);
    let dependency_artifacts = match (sources.strict_deps, mode) {
        (StrictDeps::Off, _) | (_, ClasspathMode::Off) => Vec::new(),
        _ => sources.dependency_artifacts.clone(),
    };
    inputs.extend(sources.classpath.iter().copied());
    inputs.extend(dependency_artifacts.iter().copied());
    let mut classpath_line = Line::default();
    classpath_line.add_exec_paths("--classpath", &sources.classpath);
    if mode == ClasspathMode::JavaBuilder {
        classpath_line.add("--reduce_classpath_mode");
        classpath_line.add("JAVABUILDER_REDUCED");
        classpath_line.add_exec_paths("--deps_artifacts", &dependency_artifacts);
    }
    let lines = vec![(flags.0, Params::Always), (classpath_line.0, Params::Always)];
    let command = command(state, output, compiler.command(toolchain, &runtime), lines)?;
    register_spawn(state, "JavacTurbine", message, inputs, outputs, command, base_info, Some(deps_output))
}

/// singlejar's action that makes the jar `output` of the compiled classes
/// and the resources, as Bazel's `ResourceJarActionBuilder` does.
fn resource_jar_action(
    state: &RuleState,
    toolchain: &Toolchain<'_>,
    output: Artifact,
    extras: &JavacExtras,
    resource_jars: &[Artifact],
    additional_inputs: &[Artifact],
) -> anyhow::Result<()> {
    let single_jar = toolchain.files_to_run("single_jar")?;
    let mut line = Line::default();
    for flag in ["--normalize", "--dont_change_compression", "--exclude_build_data", "--no_strip_module_info"] {
        line.add(flag);
    }
    line.add_exec_path("--output", Some(output));
    line.add_exec_paths("--sources", resource_jars);
    if !extras.resources.is_empty() {
        line.add("--resources");
        for (path, resource) in &extras.resources {
            let exec_path = resource.exec_path.to_string();
            match exec_path == *path {
                true => line.add(&exec_path),
                false => line.add(&format!("{exec_path}:{path}")),
            }
        }
    }
    line.add_exec_paths("--classpath_resources", &extras.classpath_resources);

    let mut inputs: Vec<Artifact> = extras.resources.iter().map(|(_, resource)| *resource).collect();
    inputs.extend(resource_jars.iter().copied());
    inputs.extend(additional_inputs.iter().copied());
    inputs.extend(extras.classpath_resources.iter().copied());
    inputs.push(single_jar.executable);
    let executable = vec![single_jar.executable.exec_path.to_string()];
    let command = command(state, output, executable, vec![(line.0, Params::WhenLong("shell"))])?;
    let mut execution_info = vec![("supports-path-mapping".to_string(), "1".to_string())];
    execution_info.extend(state.tag_execution_info.iter().cloned());
    let mut action = spawn_action("JavaResourceJar", "Building Java resource jar".to_string(), inputs, vec![output], command, execution_info);
    if let (false, Some(runfiles)) = (single_jar.executable.is_source(), single_jar.runfiles) {
        action.runfiles.push(runfiles_tree(single_jar.executable, runfiles));
    }
    if let ActionKind::Spawn(spawn) = &mut action.kind {
        spawn.env = environment(state.config.options.action_env.iter().cloned());
    }
    state.register(action)
}

/// GenClass's action, which makes the jar of the classes annotation
/// processors generated.
fn gen_class_action(state: &RuleState, toolchain: &Toolchain<'_>, runtime: &Runtime, class_jar: Artifact, manifest_proto: Artifact, output: Artifact) -> anyhow::Result<()> {
    let gen_class = toolchain.field("_gen_class").and_then(crate::file_value::FileValue::artifact);
    let gen_class = gen_class.ok_or_else(|| anyhow::anyhow!("the Java toolchain has no GenClass"))?;
    let mut arguments = vec![runtime.java.clone()];
    arguments.extend(toolchain.strings("jvm_opt"));
    arguments.push("-jar".to_string());
    arguments.push(gen_class.exec_path.to_string());
    for (flag, artifact) in [("--manifest_proto", manifest_proto), ("--class_jar", class_jar), ("--output_jar", output)] {
        arguments.push(flag.to_string());
        arguments.push(artifact.exec_path.to_string());
    }
    let mut inputs = vec![manifest_proto, class_jar, gen_class];
    inputs.extend(runtime.files.iter().copied());
    let command = Command {
        arguments,
        param_files: Vec::new(),
    };
    let message = format!("Building genclass jar {}", output.exec_path);
    state.register(spawn_action("JavaSourceJar", message, inputs, vec![output], command, Vec::new()))
}

/// Register a compilation's spawn, with the environment and execution
/// requirements Bazel gives Java compilations: UTF-8 text, the rule's tags,
/// and its `.jdeps` output kept in memory.
#[allow(clippy::too_many_arguments)]
fn register_spawn(
    state: &RuleState,
    mnemonic: &str,
    message: String,
    inputs: Vec<Artifact>,
    outputs: Vec<Artifact>,
    command: Command,
    mut execution_info: Vec<(String, String)>,
    deps_output: Option<Artifact>,
) -> anyhow::Result<()> {
    execution_info.extend(state.tag_execution_info.iter().cloned());
    if let (Some(deps), true) = (deps_output, state.config.options.bool("experimental_inmemory_jdeps_files")) {
        execution_info.push(("internal-inline-outputs".to_string(), deps.exec_path.to_string()));
    }
    let mut action = spawn_action(mnemonic, message, inputs, outputs, command, execution_info);
    if let ActionKind::Spawn(spawn) = &mut action.kind {
        let utf8 = ("LC_CTYPE".to_string(), utf8_locale().to_string());
        spawn.env = environment(state.config.options.action_env.iter().cloned().chain([utf8]));
    }
    state.register(action)
}

/// The locale that makes Java read and write UTF-8, as Bazel picks it for
/// the execution platform: macOS has no `C.UTF-8`.
fn utf8_locale() -> &'static str {
    match cfg!(target_os = "macos") {
        true => "en_US.UTF-8",
        false => "C.UTF-8",
    }
}

fn spawn_action(mnemonic: &str, message: String, inputs: Vec<Artifact>, outputs: Vec<Artifact>, command: Command, mut execution_info: Vec<(String, String)>) -> Action {
    execution_info.sort();
    execution_info.dedup_by(|a, b| a.0 == b.0);
    let mut inputs = inputs;
    inputs.extend(command.param_files.iter().map(|(_, artifact)| *artifact));
    Action {
        mnemonic: Symbol::intern(mnemonic),
        progress_message: message,
        inputs: InputSet::of(inputs),
        runfiles: Vec::new(),
        outputs,
        kind: ActionKind::Spawn(Spawn {
            arguments: command.arguments,
            env: Vec::new(),
            execution_info,
            param_files: command.param_files.into_iter().map(|(param_file, _)| param_file).collect(),
            directory_args: Vec::new(),
            exec_group: None,
            platform: Vec::new(),
        }),
    }
}

/// A compilation's progress message, as Bazel's Java actions describe
/// themselves: what they make, from how many sources, running which
/// annotation processors.
fn progress_message(prefix: &str, output: Artifact, sources: &Sources<'_>) -> String {
    let mut counts = Vec::new();
    for (count, what) in [(sources.source_files.len(), "source file"), (sources.source_jars.len(), "source jar")] {
        match count {
            0 => {}
            1 => counts.push(format!("1 {what}")),
            count => counts.push(format!("{count} {what}s")),
        }
    }
    let mut message = format!("{prefix} {} ({})", output.root_relative(), counts.join(", "));
    let processors = &sources.plugins.processor_classes;
    if !processors.is_empty() {
        let names: Vec<&str> = processors.iter().map(|name| name.rsplit('.').next().unwrap_or(name)).collect();
        message.push_str(&format!(" and running annotation processors ({})", names.join(", ")));
    }
    message
}

/// A path with `suffix` before its extension: `lib.jar` and `-class` make
/// `lib-class.jar`.
fn append_without_extension(path: &str, suffix: &str) -> String {
    let (dir, name) = path.rsplit_once('/').map_or(("", path), |(dir, name)| (dir, name));
    let name = match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem}{suffix}.{extension}"),
        None => format!("{name}{suffix}"),
    };
    match dir {
        "" => name,
        dir => format!("{dir}/{name}"),
    }
}

/// When a command line's arguments go in a params file.
enum Params {
    /// Always, as Bazel puts JavaBuilder's and Turbine's flags in one.
    Always,
    /// When the command would otherwise be longer than
    /// `--min_param_file_size`, in a file of the format given.
    WhenLong(&'static str),
}

/// A command's arguments, and the params files it reads with what they
/// hold.
struct Command {
    arguments: Vec<String>,
    param_files: Vec<(ParamFile, Artifact)>,
}

/// The command of an executable line and lines of arguments, each in a
/// params file as Bazel's `CommandLines` decides: always, or when the
/// command would be too long. Params files are named after `primary`,
/// `<primary>-0.params` and on.
fn command(state: &RuleState, primary: Artifact, executable: Vec<String>, lines: Vec<(Vec<String>, Params)>) -> anyhow::Result<Command> {
    let limit = match state.config.options.get("min_param_file_size") {
        Ok(bazel_config::OptionValue::Int(limit)) => *limit,
        _ => 32768,
    };
    let estimate = 512 * lines.len() as i64;
    let max_length = limit - estimate;
    let length = |arguments: &[String]| arguments.iter().map(|argument| argument.len() as i64 + 1).sum::<i64>();
    let mut command_length = length(&executable);
    let mut arguments = executable;
    let mut param_files = Vec::new();
    for (line, params) in lines {
        let format = match params {
            Params::Always => "multiline",
            Params::WhenLong(format) => {
                let tentative = command_length + length(&line);
                if tentative <= max_length {
                    command_length = tentative;
                    arguments.extend(line);
                    continue;
                }
                format
            }
        };
        let artifact = params_file(state, primary, param_files.len(), &line, format)?;
        let argument = format!("@{}", artifact.exec_path);
        command_length += argument.len() as i64 + 1;
        param_files.push((
            ParamFile {
                argument: arguments.len() as u32,
                path: artifact.exec_path,
                format: format.to_string(),
            },
            artifact,
        ));
        arguments.push(argument);
    }
    Ok(Command { arguments, param_files })
}

/// Write a params file of `arguments`, `<primary>-<index>.params`.
fn params_file(state: &RuleState, primary: Artifact, index: usize, arguments: &[String], format: &str) -> anyhow::Result<Artifact> {
    let path = format!("{}-{index}.params", primary.output_dir_relative());
    let params = state.declare_at_root(&path)?;
    let contents = crate::args::param_file_contents(arguments, format);
    state.register_own(Action {
        mnemonic: Symbol::intern("ParamFile"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![params],
        kind: ActionKind::Write {
            content: contents,
            executable: false,
            from_args: true,
            args: None,
        },
    })?;
    Ok(params)
}

/// A command line built as Bazel's `CustomCommandLine` is: flags with
/// values only when they have them.
#[derive(Default)]
struct Line(Vec<String>);

impl Line {
    fn add(&mut self, argument: &str) {
        self.0.push(argument.to_string());
    }

    fn add_optional(&mut self, flag: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.add(flag);
            self.add(value);
        }
    }

    fn add_exec_path(&mut self, flag: &str, artifact: Option<Artifact>) {
        if let Some(artifact) = artifact {
            self.add(flag);
            self.0.push(artifact.exec_path.to_string());
        }
    }

    fn add_exec_paths(&mut self, flag: &str, artifacts: &[Artifact]) {
        if artifacts.is_empty() {
            return;
        }
        self.add(flag);
        self.0.extend(artifacts.iter().map(|artifact| artifact.exec_path.to_string()));
    }

    fn add_all(&mut self, flag: &str, values: &[String]) {
        if values.is_empty() {
            return;
        }
        self.add(flag);
        self.0.extend(values.iter().cloned());
    }

    /// `--javacopts <opts> --`, with Turbine's flag for Dagger if asked.
    fn add_javac_opts(&mut self, opts: &[String], turbine_hjar: bool) {
        if opts.is_empty() && !turbine_hjar {
            return;
        }
        self.add("--javacopts");
        self.0.extend(opts.iter().cloned());
        if turbine_hjar {
            self.add("-Aexperimental_turbine_hjar");
        }
        self.add("--");
    }

    /// `--target_label`, as the main repository writes it, with another
    /// `@` before labels of other repositories, as Bazel writes them.
    fn add_target_label(&mut self, state: &RuleState, label: Label) {
        self.add("--target_label");
        let display = label.display_form(Some(&state.main_mapping));
        match label.repo().is_main() {
            true => self.0.push(display),
            false => self.0.push(format!("@{display}")),
        }
    }
}
