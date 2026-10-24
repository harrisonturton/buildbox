//! The implementations of native rules.

use crate::action::{Action, ActionKind, RunfilesTree, Spawn};
use crate::artifact::{self, Artifact};
use crate::build_ctx::{Prepared, RuleInput, target_executable, target_files};
use crate::expand::LocationStyle;
use crate::providers;
use crate::runfiles::Runfiles;
use crate::state::{RuleState, runfiles_tree};
use crate::target_value::TargetValue;
use bazel_packages::AttrValue;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::label_value::LabelValue;
use intern::Symbol;
use starlark::eval::Evaluator;
use starlark::values::list::AllocList;
use starlark::values::{Heap, Value};

/// Native rules that only exist to be referred to, such as by `select()`
/// or toolchain resolution, and provide nothing.
const MARKER_RULES: &[&str] = &[
    "config_setting",
    "config_feature_flag",
    "constraint_setting",
    "platform",
    "toolchain",
    "toolchain_type",
    "test_suite",
    "environment",
    "environment_group",
    "java_plugins_flag_alias",
    "cc_libc_top_alias",
    "cc_toolchain_suite",
];

/// Rules Bazel's builtins implement that bb stands in for with rules that
/// provide nothing, since nothing uses them any more.
pub const STUBBED_BUILTINS: &[&str] = &[];

/// Rules of Bazel's builtins whose Starlark implementations bb runs, since
/// they need only the APIs bb has.
pub const STARLARK_BUILTINS: &[&str] = &["xcode_config", "xcode_version", "available_xcodes", "xcode_config_alias"];

/// Run a native rule's implementation, returning its providers.
pub fn analyze<'v>(kind: Symbol, input: &RuleInput, prepared: &Prepared<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    let heap = eval.heap();
    match kind.as_str() {
        "filegroup" => filegroup(input, prepared, heap),
        "genrule" => genrule(input, prepared, RuleState::get(eval)?, heap),
        "constraint_value" => constraint_value(input, heap),
        "label_flag" | "label_setting" => forward(input, prepared, "build_setting_default", heap),
        "cc_toolchain" => crate::cc::rules::cc_toolchain(input, prepared, heap),
        "cc_toolchain_alias" => crate::cc::rules::cc_toolchain_alias(input, heap),
        "cc_library" => crate::cc::rules::cc_library(input, prepared, RuleState::get(eval)?, heap),
        "cc_binary" | "cc_test" => crate::cc::rules::cc_binary(input, prepared, RuleState::get(eval)?, heap),
        kind if MARKER_RULES.contains(&kind) || STUBBED_BUILTINS.contains(&kind) => Ok(Value::new_none()),
        kind => anyhow::bail!("the native rule '{kind}' isn't supported by bb yet"),
    }
}

/// A depset including others.
fn union<'v>(transitive: Vec<Value<'v>>, heap: Heap<'v>) -> Value<'v> {
    heap.alloc(Depset {
        direct: Vec::new(),
        transitive,
        order: Order::Default,
    })
}

/// A field of the `DefaultInfo` of each target.
fn default_info_fields<'v>(targets: &[Value<'v>], field: &str) -> Vec<Value<'v>> {
    targets
        .iter()
        .filter_map(|target| TargetValue::from_value(*target)?.default_info(field))
        .collect()
}

/// `filegroup`: the files of its `srcs`, or one of their output groups.
fn filegroup<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let srcs = prepared.targets(input, "srcs");
    let data = prepared.targets(input, "data");
    let group = input.string("output_group");
    let files = if group.is_empty() {
        union(default_info_fields(&srcs, "files"), heap)
    } else {
        union(output_groups(&srcs, group), heap)
    };
    let data_files = union(default_info_fields(&data, "files"), heap);
    let own = heap.alloc(Runfiles::new(union(vec![files, data_files], heap)));
    let mut all: Vec<&Runfiles<'v>> = vec![Runfiles::from_value(own).ok_or_else(|| anyhow::anyhow!("runfiles"))?];
    for runfiles in default_info_fields(&srcs, "data_runfiles").into_iter().chain(default_info_fields(&data, "data_runfiles")) {
        all.extend(Runfiles::from_value(runfiles));
    }
    let runfiles = heap.alloc(Runfiles::merge_all(&all, heap));
    let info = providers::instance("DefaultInfo", vec![("files", files), ("runfiles", runfiles)], heap)?;
    Ok(heap.alloc(AllocList([info])))
}

fn output_groups<'v>(targets: &[Value<'v>], group: &str) -> Vec<Value<'v>> {
    let mut out = Vec::new();
    for target in targets {
        let Some(groups) = TargetValue::from_value(*target).and_then(|target| target.provider("OutputGroupInfo")) else {
            continue;
        };
        if let Some(files) = bazel_starlark::provider::ProviderInstance::from_value(groups).and_then(|groups| groups.field(group)) {
            out.push(files);
        }
    }
    out
}

/// `constraint_value`: its label and setting, as `ConstraintValueInfo`.
fn constraint_value<'v>(input: &RuleInput, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let setting = match input.attr("constraint_setting") {
        Some(AttrValue::Label(setting)) => heap.alloc(LabelValue(*setting)),
        _ => Value::new_none(),
    };
    let info = providers::instance(
        "ConstraintValueInfo",
        vec![("label", heap.alloc(LabelValue(input.label))), ("constraint_setting", setting)],
        heap,
    )?;
    Ok(heap.alloc(AllocList([info])))
}

/// The providers of the target an attribute names, as `label_flag` has.
fn forward<'v>(input: &RuleInput, prepared: &Prepared<'v>, attr: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let Some(target) = prepared.targets(input, attr).first().and_then(|target| TargetValue::from_value(*target)) else {
        return Ok(Value::new_none());
    };
    Ok(heap.alloc(AllocList(target.providers.values().copied())))
}

/// The tags that become execution requirements, as Bazel propagates them.
fn execution_info(input: &RuleInput) -> Vec<(String, String)> {
    let tags = match input.attr("tags") {
        Some(AttrValue::StringList(tags)) => tags.clone(),
        _ => Vec::new(),
    };
    let mut info: Vec<(String, String)> = tags
        .into_iter()
        .filter(|tag| {
            ["no-", "requires-", "supports-", "block-network", "local", "cpu:"]
                .iter()
                .any(|prefix| tag.starts_with(prefix))
        })
        .map(|tag| (tag, String::new()))
        .collect();
    if input.bool("local") {
        info.push(("local".to_string(), String::new()));
    }
    info.sort();
    info.dedup();
    info
}

/// The tools of a genrule: their files, and their executables' runfiles.
fn genrule_tools(input: &RuleInput, prepared: &Prepared<'_>) -> anyhow::Result<(Vec<Artifact>, Vec<RunfilesTree>)> {
    let mut inputs = Vec::new();
    let mut trees = Vec::new();
    for attr in ["tools", "exec_tools", "toolchains"] {
        for target in prepared.targets(input, attr) {
            inputs.extend(target_files(target)?);
            if let Some((executable, runfiles)) = target_executable(target) {
                inputs.push(executable);
                trees.push(runfiles_tree(executable, runfiles));
            }
        }
    }
    Ok((inputs, trees))
}

fn paths(artifacts: &[Artifact]) -> String {
    artifacts.iter().map(|artifact| artifact.exec_path.to_string()).collect::<Vec<_>>().join(" ")
}

/// The variables a genrule's command can use besides the make variables:
/// `$@`, `$<`, `$(SRCS)` and the like.
fn genrule_variables(input: &RuleInput, srcs: &[Artifact], outs: &[Artifact]) -> Vec<(String, String)> {
    let rule_dir = artifact::parse(&input.config.bin_dir()).join_path(artifact::package_dir(input.label.package)).to_string();
    let mut variables = vec![
        ("SRCS".to_string(), paths(srcs)),
        ("^".to_string(), paths(srcs)),
        ("OUTS".to_string(), paths(outs)),
        ("RULEDIR".to_string(), rule_dir.clone()),
    ];
    if let [out] = outs {
        variables.push(("@".to_string(), out.exec_path.to_string()));
        let dir = out.exec_path.parent().map(|parent| parent.to_string()).unwrap_or_default();
        variables.push(("@D".to_string(), dir));
    } else {
        variables.push(("@D".to_string(), rule_dir));
    }
    if let [src] = srcs {
        variables.push(("<".to_string(), src.exec_path.to_string()));
    }
    variables
}

/// `genrule`: run a shell command.
fn genrule<'v>(input: &RuleInput, prepared: &Prepared<'v>, state: &RuleState, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let srcs = prepared.files(input, "srcs")?;
    let outs = prepared.outputs.attr("outs");
    let (tools, runfiles) = genrule_tools(input, prepared)?;
    let command = match input.string("cmd_bash") {
        "" => input.string("cmd"),
        command => command,
    };
    if command.is_empty() {
        anyhow::bail!("missing value for 'cmd' attribute");
    }
    let variables = genrule_variables(input, &srcs, &outs);
    let expanded = state.expand_with(command, &[], &variables, LocationStyle::Exec)?;

    let setup = match input.attr("$genrule_setup") {
        Some(AttrValue::Label(label)) => Artifact::source(*label),
        _ => anyhow::bail!("genrule has no setup script"),
    };
    let mut inputs = srcs.clone();
    inputs.extend(tools);
    inputs.push(setup);
    inputs.sort();
    inputs.dedup();
    let script = format!("source {}; {expanded}", setup.exec_path);
    let message = match input.string("message") {
        "" => "Executing genrule",
        message => message,
    };
    state.register(Action {
        mnemonic: Symbol::intern("Genrule"),
        progress_message: format!("{message} {}", input.label),
        inputs,
        runfiles,
        outputs: outs.clone(),
        kind: ActionKind::Spawn(Spawn {
            arguments: vec!["/bin/bash".to_string(), "-c".to_string(), script],
            env: input.config.options.action_env.clone(),
            execution_info: execution_info(input),
        }),
    })?;

    let executable = match (input.bool("executable"), outs.as_slice()) {
        (true, [out]) => Some(*out),
        (true, _) => anyhow::bail!("genrule with executable = True must have exactly one output"),
        (false, _) => None,
    };
    let files = providers::file_depset(&outs, heap);
    let mut fields = vec![("files", files)];
    if let Some(executable) = executable {
        fields.push(("executable", heap.alloc(crate::file_value::FileValue(executable))));
    }
    let data = prepared.targets(input, "data");
    let mut all = Vec::new();
    for runfiles in default_info_fields(&data, "data_runfiles") {
        all.extend(Runfiles::from_value(runfiles));
    }
    let own = heap.alloc(Runfiles::new(files));
    all.insert(0, Runfiles::from_value(own).ok_or_else(|| anyhow::anyhow!("runfiles"))?);
    fields.push(("runfiles", heap.alloc(Runfiles::merge_all(&all, heap))));
    let info = providers::instance("DefaultInfo", fields, heap)?;
    Ok(heap.alloc(AllocList([info])))
}
