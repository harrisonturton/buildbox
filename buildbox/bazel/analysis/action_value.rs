//! `Action`: an action a target registered, as `Target.actions` lists
//! them, mostly for analysis tests.

use crate::action::{Action, ActionKind};
use crate::configured::ConfiguredTarget;
use crate::providers::file_depset;
use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, starlark_value};
use std::cell::RefCell;
use std::fmt;
use std::sync::Arc;

/// One action, with what Starlark can read of it.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct ActionValue {
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    action: Action,
}

starlark_simple_value!(ActionValue);

impl fmt::Display for ActionValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<action {}>", self.action.mnemonic)
    }
}

const FIELDS: [&str; 8] = ["argv", "content", "env", "execution_info", "inputs", "mnemonic", "outputs", "substitutions"];

#[starlark_value(type = "Action")]
impl<'v> StarlarkValue<'v> for ActionValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        let action = &self.action;
        let value = match attribute {
            "mnemonic" => heap.alloc(action.mnemonic.as_str()),
            "inputs" => file_depset(&action.all_inputs(), heap),
            "outputs" => file_depset(&action.outputs, heap),
            "argv" => match &action.kind {
                ActionKind::Spawn(spawn) => heap.alloc(AllocList(spawn.arguments.iter().map(String::as_str))),
                _ => Value::new_none(),
            },
            "content" => match &action.kind {
                ActionKind::Write { content, .. } => heap.alloc(content.as_str()),
                _ => Value::new_none(),
            },
            "substitutions" => match &action.kind {
                ActionKind::Template { substitutions, .. } => string_dict(substitutions, heap),
                _ => Value::new_none(),
            },
            "env" => match &action.kind {
                ActionKind::Spawn(spawn) => string_dict(&spawn.env, heap),
                _ => string_dict(&[], heap),
            },
            "execution_info" => match &action.kind {
                ActionKind::Spawn(spawn) => string_dict(&spawn.execution_info, heap),
                _ => string_dict(&[], heap),
            },
            _ => return None,
        };
        Some(value)
    }

    fn dir_attr(&self) -> Vec<String> {
        FIELDS.iter().map(|field| field.to_string()).collect()
    }
}

fn string_dict<'v>(entries: &[(String, String)], heap: Heap<'v>) -> Value<'v> {
    heap.alloc(AllocDict(entries.iter().map(|(key, value)| (key.as_str(), value.as_str()))))
}

thread_local! {
    /// The configured targets each `Target` value of the rule being
    /// analyzed stands for, by the value's address.
    static DEP_TARGETS: RefCell<Vec<(usize, Vec<Arc<ConfiguredTarget>>)>> = const { RefCell::new(Vec::new()) };
}

/// While alive, the `Target` values registered with it can answer what
/// only their configured targets know, such as `Target.actions`.
pub struct DepTargets {
    previous: Vec<(usize, Vec<Arc<ConfiguredTarget>>)>,
}

impl DepTargets {
    /// Pair each `Target` value with the configured targets it stands for:
    /// a target, or the applications of aspects to it and then itself.
    pub fn begin(targets: Vec<(usize, Vec<Arc<ConfiguredTarget>>)>) -> DepTargets {
        DepTargets {
            previous: DEP_TARGETS.replace(targets),
        }
    }
}

impl Drop for DepTargets {
    fn drop(&mut self) {
        DEP_TARGETS.set(std::mem::take(&mut self.previous));
    }
}

/// The configured targets [`DepTargets`] pairs the `Target` value at
/// `address` with.
fn dep_targets(address: usize) -> Option<Vec<Arc<ConfiguredTarget>>> {
    DEP_TARGETS.with_borrow(|registered| registered.iter().find(|(registered, _)| *registered == address).map(|(_, targets)| targets.clone()))
}

/// The kind of rule of the `Target` value at `address`, such as
/// `java_library`, if [`DepTargets`] pairs it with a target.
pub fn target_kind(address: usize) -> Option<intern::Symbol> {
    dep_targets(address)?.last().map(|target| target.kind)
}

/// `Target.actions` of the `Target` value at `address`: the actions of the
/// targets [`DepTargets`] pairs it with, or none if it pairs it with none.
pub fn target_actions<'v>(address: usize, heap: Heap<'v>) -> Option<Value<'v>> {
    let targets = dep_targets(address)?;
    let mut actions = Vec::new();
    for target in &targets {
        for stored in &target.actions {
            let action = stored.load().ok()?;
            actions.push(heap.alloc(ActionValue { action }));
        }
    }
    Some(heap.alloc(AllocList(actions)))
}
