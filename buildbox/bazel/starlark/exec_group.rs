//! `exec_group()`: a group of a rule's actions and tools that runs on an
//! execution platform of its own.

use crate::context::EvalContext;
use crate::convert::LabelParser;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::ExecGroup;
use intern::Symbol;
use skycodec::{Decode, Encode};
use starlark::eval::Evaluator;
use starlark::starlark_simple_value;
use starlark::values::dict::DictRef;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// What `exec_group()` returns: the toolchains the group's execution
/// platform must have, and its constraints.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct ExecGroupValue {
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub toolchain_types: Vec<Label>,
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub optional_toolchain_types: Vec<Label>,
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub exec_compatible_with: Vec<Label>,
}

starlark_simple_value!(ExecGroupValue);

impl fmt::Display for ExecGroupValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<exec_group>")
    }
}

#[starlark_value(type = "ExecGroup")]
impl<'v> StarlarkValue<'v> for ExecGroupValue {}

/// `exec_group(toolchains = [], exec_compatible_with = [])`, with labels
/// relative to the calling file.
pub fn exec_group<'v>(toolchains: Vec<Value<'v>>, exec_compatible_with: Vec<Value<'v>>, eval: &Evaluator<'v, '_, '_>) -> anyhow::Result<ExecGroupValue> {
    let context = EvalContext::get(eval)?;
    let caller = context.caller_file(eval);
    let mapping = context.mapping_for(caller.repo());
    let parser = LabelParser {
        package: caller.package,
        mapping: &*mapping,
    };
    let (toolchain_types, optional_toolchain_types) = crate::rule_value::toolchain_labels(toolchains, &parser)?;
    let mut constraints = Vec::with_capacity(exec_compatible_with.len());
    for constraint in exec_compatible_with {
        constraints.push(parser.label(constraint)?);
    }
    Ok(ExecGroupValue {
        toolchain_types,
        optional_toolchain_types,
        exec_compatible_with: constraints,
    })
}

/// The exec groups `exec_groups = {name: exec_group(), ...}` declares, for
/// a rule or an aspect, as `what` says.
pub(crate) fn exec_groups(groups: Option<Value<'_>>, what: &str) -> anyhow::Result<Vec<ExecGroup>> {
    let Some(groups) = groups.filter(|groups| !groups.is_none()) else {
        return Ok(Vec::new());
    };
    let Some(groups) = DictRef::from_value(groups) else {
        anyhow::bail!("{what}: exec_groups must be a dict, not {}", groups.get_type());
    };
    let mut declared = Vec::with_capacity(groups.len());
    for (name, group) in groups.iter() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("{what}: exec group names must be strings, not {}", name.get_type());
        };
        if !is_valid_name(name) {
            anyhow::bail!("Exec group name '{name}' is not a valid name.");
        }
        let Some(group) = group.downcast_ref::<ExecGroupValue>() else {
            anyhow::bail!("expected type 'exec_group' for 'exec_group' value, got '{}'", group.get_type());
        };
        declared.push(ExecGroup {
            name: Symbol::intern(name),
            toolchain_types: group.toolchain_types.clone(),
            optional_toolchain_types: group.optional_toolchain_types.clone(),
            exec_compatible_with: group.exec_compatible_with.clone(),
        });
    }
    Ok(declared)
}

/// The `test` exec group Bazel gives test rules that don't declare one:
/// its optional toolchain makes test actions prefer an execution platform
/// with the target platform's constraints.
pub fn default_test_group() -> ExecGroup {
    let toolchain_type = Label::parse_canonical("@@bazel_tools//tools/test:default_test_toolchain_type").expect("a valid label");
    ExecGroup {
        name: Symbol::intern("test"),
        toolchain_types: vec![toolchain_type],
        optional_toolchain_types: vec![toolchain_type],
        exec_compatible_with: Vec::new(),
    }
}

/// Whether an exec group can have this name: it must be an identifier.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    starts_well && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

