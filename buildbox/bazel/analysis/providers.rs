//! The providers a rule returns, gathered into its `Target`.

use crate::artifact::Artifact;
use crate::file_value::FileValue;
use crate::runfiles::{FilesToRun, Runfiles};
use crate::target_value::{DEFAULT_INFO, TargetValue};
use bazel_label::Label;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::opaque::Opaque;
use bazel_starlark::provider::ProviderInstance;
use starlark::collections::SmallMap;
use starlark::values::{Heap, Value, ValueLike};

/// A builtin provider, such as `DefaultInfo`. Builtin providers are never
/// freed, so any heap may refer to them.
pub fn builtin<'v>(name: &str) -> anyhow::Result<Value<'v>> {
    let (provider, _) = bazel_starlark::provider::builtin_provider(name).ok_or_else(|| anyhow::anyhow!("no builtin provider {name}"))?;
    Ok(provider.to_value())
}

/// An instance of a builtin provider.
pub fn instance<'v>(name: &str, fields: Vec<(&str, Value<'v>)>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let provider = builtin(name)?;
    let fields = fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect();
    Ok(heap.alloc(ProviderInstance { provider, fields }))
}

/// A depset of files.
pub fn file_depset<'v>(files: &[Artifact], heap: Heap<'v>) -> Value<'v> {
    heap.alloc(Depset {
        direct: files.iter().map(|artifact| heap.alloc(FileValue(*artifact))).collect(),
        transitive: Vec::new(),
        order: Order::Default,
    })
}

/// What a rule's `DefaultInfo` defaults to.
pub struct Defaults {
    /// The predeclared outputs actions generate.
    pub files: Vec<Artifact>,
    /// The predeclared executable, if the rule is executable and an action
    /// generates it.
    pub executable: Option<Artifact>,
}

/// Gather what a rule implementation returned into its `Target`: a list
/// of providers, one provider, or `None`.
pub fn collect<'v>(label: Label, returned: Value<'v>, defaults: &Defaults, heap: Heap<'v>) -> anyhow::Result<TargetValue<'v>> {
    let items = if returned.is_none() {
        Vec::new()
    } else if ProviderInstance::from_value(returned).is_some() || native_provider_key(returned).is_some() {
        vec![returned]
    } else {
        bazel_starlark::convert::items(returned).ok_or_else(|| {
            anyhow::anyhow!("{label}: a rule implementation must return a list of providers, not {}", returned.get_type())
        })?
    };
    let mut providers = SmallMap::new();
    for item in items {
        if item.downcast_ref::<Opaque>().is_some() {
            continue;
        }
        let key = match ProviderInstance::from_value(item) {
            Some(instance) => instance.provider_key().unwrap_or_default(),
            None => match native_provider_key(item) {
                Some(key) => key.to_string(),
                None => anyhow::bail!("{label}: a rule implementation must return providers, not {}", item.get_type()),
            },
        };
        if providers.insert(key.clone(), item).is_some() {
            anyhow::bail!("{label}: the rule returned more than one {key}");
        }
    }
    let given = providers.get(DEFAULT_INFO).copied();
    let default_info = default_info(label, given, defaults, heap)?;
    providers.insert(DEFAULT_INFO.to_string(), default_info);
    Ok(TargetValue { label, providers })
}

/// The provider an instance of a natively implemented provider, such as a
/// `CcInfo`, is of.
fn native_provider_key(value: Value<'_>) -> Option<&'static str> {
    use crate::cc::values::{CcStruct, CcToolchain, ToolchainConfigValue};
    if let Some(value) = CcStruct::from_value(value) {
        return (value.kind == "CcInfo").then_some("CcInfo");
    }
    if CcToolchain::from_value(value).is_some() {
        return Some("CcToolchainInfo");
    }
    value.downcast_ref::<ToolchainConfigValue>().map(|_| "CcToolchainConfigInfo")
}

/// A complete `DefaultInfo`, from one a rule returned, with what it left
/// out filled in.
fn default_info<'v>(label: Label, given: Option<Value<'v>>, defaults: &Defaults, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let given = given.and_then(ProviderInstance::from_value);
    let field = |name: &str| given.and_then(|instance| instance.field(name));

    let executable = match field("executable") {
        Some(value) => Some(FileValue::artifact(value).ok_or_else(|| anyhow::anyhow!("{label}: DefaultInfo.executable must be a File"))?),
        None => defaults.executable,
    };
    let files = match field("files") {
        Some(files) if Depset::from_value(files).is_some() => files,
        Some(files) => anyhow::bail!("{label}: DefaultInfo.files must be a depset, not {}", files.get_type()),
        None => {
            let mut files = defaults.files.clone();
            files.extend(executable.filter(|executable| !files.contains(executable)));
            file_depset(&files, heap)
        }
    };
    let runfiles = field("runfiles").or_else(|| field("default_runfiles"));
    let default_runfiles = with_executable(runfiles, executable, heap)?;
    let data_runfiles = match field("data_runfiles") {
        Some(runfiles) => with_executable(Some(runfiles), executable, heap)?,
        None => default_runfiles,
    };
    let executable_value = executable.map(|artifact| heap.alloc(FileValue(artifact))).unwrap_or_else(Value::new_none);
    let files_to_run = heap.alloc(FilesToRun {
        executable: executable_value,
        runfiles: default_runfiles,
    });
    instance(
        DEFAULT_INFO,
        vec![
            ("files", files),
            ("default_runfiles", default_runfiles),
            ("data_runfiles", data_runfiles),
            ("files_to_run", files_to_run),
            ("executable", executable_value),
        ],
        heap,
    )
}

/// Runfiles, with the executable they're for included.
fn with_executable<'v>(runfiles: Option<Value<'v>>, executable: Option<Artifact>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let runfiles = match runfiles {
        Some(value) => Runfiles::from_value(value).ok_or_else(|| anyhow::anyhow!("expected runfiles, not {}", value.get_type()))?,
        None => {
            let empty = heap.alloc(Runfiles::empty(heap));
            Runfiles::from_value(empty).ok_or_else(|| anyhow::anyhow!("runfiles"))?
        }
    };
    let Some(executable) = executable else {
        return Ok(heap.alloc(Runfiles::merge_all(&[runfiles], heap)));
    };
    let own = heap.alloc(Runfiles::new(file_depset(&[executable], heap)));
    let own = Runfiles::from_value(own).ok_or_else(|| anyhow::anyhow!("runfiles"))?;
    Ok(heap.alloc(Runfiles::merge_all(&[own, runfiles], heap)))
}

/// The `Target` of a file: its `DefaultInfo` has just the file, as its
/// files and its runfiles.
pub fn file_target<'v>(label: Label, artifact: Artifact, heap: Heap<'v>) -> anyhow::Result<TargetValue<'v>> {
    let files = file_depset(&[artifact], heap);
    let runfiles = heap.alloc(Runfiles::new(files));
    // A file runs as itself, as an executable attribute's value, but isn't
    // an executable target: it has no runfiles tree of its own.
    let files_to_run = heap.alloc(FilesToRun {
        executable: heap.alloc(FileValue(artifact)),
        runfiles,
    });
    let fields = vec![
        ("files", files),
        ("default_runfiles", runfiles),
        ("data_runfiles", runfiles),
        ("files_to_run", files_to_run),
        ("executable", Value::new_none()),
    ];
    let mut providers = SmallMap::new();
    providers.insert(DEFAULT_INFO.to_string(), instance(DEFAULT_INFO, fields, heap)?);
    Ok(TargetValue { label, providers })
}
