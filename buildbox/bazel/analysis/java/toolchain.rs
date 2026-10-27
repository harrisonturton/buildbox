//! The parts of rules_java's providers that Java compilation reads: the
//! toolchain, its runtime and tools, the bootclasspath and plugins.

use crate::artifact::Artifact;
use crate::file_value::FileValue;
use crate::runfiles::{FilesToRun, Runfiles};
use bazel_starlark::depset::Depset;
use starlark::values::{Heap, Value};

/// A field of a provider instance or struct, or `None` if it's missing or
/// `None`.
pub fn field<'v>(value: Value<'v>, name: &str, heap: Heap<'v>) -> Option<Value<'v>> {
    value.get_attr(name, heap).ok().flatten().filter(|value| !value.is_none())
}

/// The files of a depset, in its order, or of a list.
pub fn files(value: Option<Value<'_>>) -> Vec<Artifact> {
    items(value).into_iter().filter_map(FileValue::artifact).collect()
}

/// The strings of a depset, in its order, or of a list.
pub fn strings(value: Option<Value<'_>>) -> Vec<String> {
    items(value).into_iter().filter_map(|item| item.unpack_str().map(str::to_string)).collect()
}

fn items(value: Option<Value<'_>>) -> Vec<Value<'_>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Vec::new();
    };
    match Depset::from_value(value) {
        Some(depset) => depset.to_list(),
        None => bazel_starlark::convert::items(value).unwrap_or_default(),
    }
}

/// A `JavaToolchainInfo`.
pub struct Toolchain<'v> {
    pub info: Value<'v>,
    pub heap: Heap<'v>,
}

impl<'v> Toolchain<'v> {
    pub fn field(&self, name: &str) -> Option<Value<'v>> {
        field(self.info, name, self.heap)
    }

    pub fn files(&self, name: &str) -> Vec<Artifact> {
        files(self.field(name))
    }

    pub fn strings(&self, name: &str) -> Vec<String> {
        strings(self.field(name))
    }

    pub fn bool(&self, name: &str) -> bool {
        self.field(name).is_some_and(|value| value.to_bool())
    }

    /// One of the toolchain's tools, such as `_javabuilder`.
    pub fn tool(&self, name: &str) -> anyhow::Result<Option<Tool<'v>>> {
        let Some(tool) = self.field(name) else {
            return Ok(None);
        };
        let files_to_run = field(tool, "tool", self.heap).ok_or_else(|| anyhow::anyhow!("the Java toolchain's {name} has no tool"))?;
        Ok(Some(Tool {
            files_to_run: FilesToRunOf::of(files_to_run, name)?,
            data: files(field(tool, "data", self.heap)),
            jvm_opts: strings(field(tool, "jvm_opts", self.heap)),
        }))
    }

    /// An executable of the toolchain given as `FilesToRunProvider`, such
    /// as `single_jar`.
    pub fn files_to_run(&self, name: &str) -> anyhow::Result<FilesToRunOf<'v>> {
        let value = self.field(name).ok_or_else(|| anyhow::anyhow!("the Java toolchain has no {name}"))?;
        FilesToRunOf::of(value, name)
    }

    /// The Java runtime that runs the toolchain's tools.
    pub fn runtime(&self) -> anyhow::Result<Runtime> {
        let runtime = self.field("java_runtime").ok_or_else(|| anyhow::anyhow!("the Java toolchain has no java_runtime"))?;
        let java = field(runtime, "java_executable_exec_path", self.heap).and_then(|path| path.unpack_str().map(str::to_string));
        Ok(Runtime {
            files: files(field(runtime, "files", self.heap)),
            java: java.ok_or_else(|| anyhow::anyhow!("the Java runtime has no java_executable_exec_path"))?,
        })
    }

    /// The toolchain's default bootclasspath.
    pub fn bootclasspath(&self) -> BootClassPath {
        match self.field("_bootclasspath_info") {
            Some(info) => BootClassPath::of(info, self.heap),
            None => BootClassPath::default(),
        }
    }
}

/// An executable, and its runfiles, as a `FilesToRunProvider` gives them.
pub struct FilesToRunOf<'v> {
    pub executable: Artifact,
    pub runfiles: Option<&'v Runfiles<'v>>,
}

impl<'v> FilesToRunOf<'v> {
    fn of(value: Value<'v>, what: &str) -> anyhow::Result<FilesToRunOf<'v>> {
        let files_to_run = FilesToRun::from_value(value).ok_or_else(|| anyhow::anyhow!("the Java toolchain's {what} isn't a FilesToRunProvider"))?;
        let executable = FileValue::artifact(files_to_run.executable).ok_or_else(|| anyhow::anyhow!("the Java toolchain's {what} has no executable"))?;
        Ok(FilesToRunOf {
            executable,
            runfiles: Runfiles::from_value(files_to_run.runfiles),
        })
    }
}

/// A tool of the toolchain, as Bazel's `JavaToolchainTool` is: an
/// executable, maybe a deploy jar the toolchain's runtime runs.
pub struct Tool<'v> {
    pub files_to_run: FilesToRunOf<'v>,
    pub data: Vec<Artifact>,
    pub jvm_opts: Vec<String>,
}

impl Tool<'_> {
    fn is_jar(&self) -> bool {
        self.files_to_run.executable.exec_path.name().as_str().ends_with(".jar")
    }

    /// The command that runs it: `java [jvm flags] -jar tool.jar` for a
    /// deploy jar.
    pub fn command(&self, toolchain: &Toolchain<'_>, runtime: &Runtime) -> Vec<String> {
        let executable = self.files_to_run.executable.exec_path.to_string();
        if !self.is_jar() {
            let mut command = vec![executable];
            command.extend(self.jvm_opts.iter().cloned());
            return command;
        }
        let mut command = vec![runtime.java.clone()];
        command.extend(toolchain.strings("jvm_opt"));
        command.extend(self.jvm_opts.iter().cloned());
        command.push("-jar".to_string());
        command.push(executable);
        command
    }

    /// The files it needs: its data, its executable, and for a deploy jar,
    /// the runtime. Bazel leaves out its runfiles.
    pub fn inputs(&self, runtime: &Runtime) -> Vec<Artifact> {
        let mut inputs = self.data.clone();
        inputs.push(self.files_to_run.executable);
        if self.is_jar() {
            inputs.extend(runtime.files.iter().copied());
        }
        inputs
    }
}

/// A Java runtime: its files, and its `java` binary.
pub struct Runtime {
    pub files: Vec<Artifact>,
    pub java: String,
}

/// A `BootClassPathInfo`.
#[derive(Default)]
pub struct BootClassPath {
    pub bootclasspath: Vec<Artifact>,
    pub auxiliary: Vec<Artifact>,
    pub system_inputs: Vec<Artifact>,
    pub system_path: Option<String>,
}

impl BootClassPath {
    pub fn of<'v>(info: Value<'v>, heap: Heap<'v>) -> BootClassPath {
        BootClassPath {
            bootclasspath: files(field(info, "bootclasspath", heap)),
            auxiliary: files(field(info, "_auxiliary", heap)),
            system_inputs: files(field(info, "_system_inputs", heap)),
            system_path: field(info, "_system_path", heap).and_then(|path| path.unpack_str().map(str::to_string)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bootclasspath.is_empty() && self.auxiliary.is_empty() && self.system_inputs.is_empty() && self.system_path.is_none()
    }
}

/// The annotation processors of a `JavaPluginInfo`, as Bazel's
/// `JavaPluginData` holds them.
#[derive(Default)]
pub struct Plugins {
    pub processor_classes: Vec<String>,
    pub processor_jars: Vec<Artifact>,
    pub processor_data: Vec<Artifact>,
}

impl Plugins {
    /// The plugins of a `JavaPluginInfo`'s field, `plugins` or
    /// `api_generating_plugins`.
    pub fn of<'v>(info: Value<'v>, which: &str, heap: Heap<'v>) -> Plugins {
        let Some(data) = field(info, which, heap) else {
            return Plugins::default();
        };
        Plugins {
            processor_classes: strings(field(data, "processor_classes", heap)),
            processor_jars: files(field(data, "processor_jars", heap)),
            processor_data: files(field(data, "processor_data", heap)),
        }
    }
}
