//! Bazel 8's symbolic macros, run as legacy macros.

use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// A macro defined with `macro()`. Calling it calls its implementation with
/// the same keyword arguments, and `visibility = None` if not given; the
/// macro's own scoping of names and visibility isn't enforced.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct SymbolicMacroGen<V: ValueLifetimeless> {
    pub implementation: V,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<SymbolicMacroGen<To>> for SymbolicMacroGen<From> {}

starlark_complex_value!(pub SymbolicMacro);

impl<'v, V: ValueLike<'v>> fmt::Display for SymbolicMacroGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<macro>")
    }
}

#[starlark_value(type = "macro")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SymbolicMacroGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let mut kwargs: Vec<(&str, Value<'v>)> = names.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        if !kwargs.iter().any(|(name, _)| *name == "visibility") {
            kwargs.push(("visibility", Value::new_none()));
        }
        eval.eval_function(self.implementation.to_value(), &[], &kwargs)
    }
}
