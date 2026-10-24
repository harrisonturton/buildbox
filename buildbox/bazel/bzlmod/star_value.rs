//! Starlark values kept outside an evaluation, such as tag attributes.

use bazel_label::Label;
use bazel_starlark::label_value::LabelValue;
use skycodec::{Decode, Encode};
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, Value, ValueLike};

/// A plain Starlark value: what MODULE.bazel files and module extensions
/// pass around as attribute values.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum StarValue {
    None,
    Bool(bool),
    Int(i64),
    String(String),
    Label(Label),
    List(Vec<StarValue>),
    Dict(Vec<(StarValue, StarValue)>),
}

impl StarValue {
    /// Copy a value out of a Starlark heap.
    pub fn from_value(value: Value<'_>) -> anyhow::Result<StarValue> {
        if value.is_none() {
            return Ok(StarValue::None);
        }
        if let Some(b) = value.unpack_bool() {
            return Ok(StarValue::Bool(b));
        }
        if let Some(i) = value.unpack_i32() {
            return Ok(StarValue::Int(i as i64));
        }
        if let Some(s) = value.unpack_str() {
            return Ok(StarValue::String(s.to_string()));
        }
        if let Some(label) = value.downcast_ref::<LabelValue>() {
            return Ok(StarValue::Label(label.0));
        }
        if let Some(list) = ListRef::from_value(value) {
            return Ok(StarValue::List(list.iter().map(StarValue::from_value).collect::<anyhow::Result<_>>()?));
        }
        if let Some(tuple) = TupleRef::from_value(value) {
            return Ok(StarValue::List(tuple.iter().map(StarValue::from_value).collect::<anyhow::Result<_>>()?));
        }
        if let Some(dict) = DictRef::from_value(value) {
            let mut entries = Vec::with_capacity(dict.len());
            for (k, v) in dict.iter() {
                entries.push((StarValue::from_value(k)?, StarValue::from_value(v)?));
            }
            return Ok(StarValue::Dict(entries));
        }
        anyhow::bail!("values of type '{}' can't be used here: {}", value.get_type(), value.to_repr())
    }

    /// Allocate the value in a Starlark heap.
    pub fn to_value<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        match self {
            StarValue::None => Value::new_none(),
            StarValue::Bool(b) => Value::new_bool(*b),
            StarValue::Int(i) => heap.alloc(*i as i32),
            StarValue::String(s) => heap.alloc(s.as_str()),
            StarValue::Label(label) => heap.alloc(LabelValue(*label)),
            StarValue::List(items) => heap.alloc(AllocList(items.iter().map(|item| item.to_value(heap)))),
            StarValue::Dict(entries) => heap.alloc(AllocDict(entries.iter().map(|(k, v)| (k.to_value(heap), v.to_value(heap))))),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            StarValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[StarValue]> {
        match self {
            StarValue::List(items) => Some(items),
            _ => None,
        }
    }
}
