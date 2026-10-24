//! Build variables from Starlark values, as Bazel's `CcToolchainVariables`
//! reads the dicts rules_cc builds them from.

use super::features::{Variable, Variables};
use crate::file_value::FileValue;
use bazel_starlark::depset::Depset;
use bazel_starlark::label_value::LabelValue;
use starlark::values::dict::DictRef;
use starlark::values::{Heap, Value, ValueLike};
use std::collections::BTreeMap;

/// How deeply structures are read, as a guard against cycles.
const MAX_DEPTH: usize = 16;

/// The variables a dict of names to values gives. Values that can't be
/// variables are left out, as `None` is.
pub fn from_dict<'v>(dict: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Variables> {
    let Some(dict) = DictRef::from_value(dict) else {
        anyhow::bail!("expected a dict of build variables, got {}", dict.get_type());
    };
    let mut variables = Variables::default();
    for (name, value) in dict.iter() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("build variable names must be strings, not {}", name.get_type());
        };
        if let Some(variable) = variable(value, heap, 0) {
            variables.set(name, variable);
        }
    }
    Ok(variables)
}

/// A value as a variable: strings, booleans, labels and files as strings
/// or booleans, sequences and depsets as sequences, and structs and
/// providers as structures of their fields.
pub fn variable<'v>(value: Value<'v>, heap: Heap<'v>, depth: usize) -> Option<Variable> {
    if value.is_none() || depth > MAX_DEPTH {
        return None;
    }
    if let Some(text) = value.unpack_str() {
        return Some(Variable::String(text.to_string()));
    }
    if let Some(flag) = value.unpack_bool() {
        return Some(Variable::Bool(flag));
    }
    if let Some(label) = value.downcast_ref::<LabelValue>() {
        return Some(Variable::String(label.0.to_string()));
    }
    if let Some(artifact) = FileValue::artifact(value) {
        return Some(Variable::String(artifact.exec_path.to_string()));
    }
    if let Some(depset) = Depset::from_value(value) {
        return Some(sequence(depset.to_list(), heap, depth));
    }
    if let Some(items) = bazel_starlark::convert::items(value) {
        return Some(sequence(items, heap, depth));
    }
    structure(value, heap, depth)
}

fn sequence<'v>(items: Vec<Value<'v>>, heap: Heap<'v>, depth: usize) -> Variable {
    Variable::Sequence(items.into_iter().filter_map(|item| variable(item, heap, depth + 1)).collect())
}

/// A struct or provider instance as a structure of its fields.
fn structure<'v>(value: Value<'v>, heap: Heap<'v>, depth: usize) -> Option<Variable> {
    let names = value.dir_attr();
    if names.is_empty() {
        return None;
    }
    let mut fields = BTreeMap::new();
    for name in names {
        let Ok(Some(field)) = value.get_attr(&name, heap) else {
            continue;
        };
        if let Some(variable) = variable(field, heap, depth + 1) {
            fields.insert(name, variable);
        }
    }
    Some(Variable::Structure(fields))
}

/// `parent`, with each of `children` on top, later ones taking precedence.
pub fn combine(parent: &Variables, children: &[&Variables]) -> Variables {
    let mut combined = parent.clone();
    for child in children {
        for (name, value) in &child.values {
            combined.set(name, value.clone());
        }
    }
    combined
}
