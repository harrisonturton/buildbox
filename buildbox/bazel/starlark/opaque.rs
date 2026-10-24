//! Values that only need to exist, not yet do anything, while loading.

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, starlark_value};
use std::fmt;

/// A value of a Bazel type that loading creates but doesn't look inside,
/// such as an aspect or a transition. It has the type's name, and any
/// attribute of it is `None`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Opaque {
    pub type_name: &'static str,
}

starlark_simple_value!(Opaque);

impl Opaque {
    pub fn new(type_name: &'static str) -> Opaque {
        Opaque { type_name }
    }
}

impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.type_name)
    }
}

#[starlark_value(type = "opaque")]
impl<'v> StarlarkValue<'v> for Opaque {
    fn get_attr(&self, _attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        Some(Value::new_none())
    }
}
