//! `select()`, and concatenations of values with selects.

use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// `select({condition: value, ...})`: a value chosen by configuration.
#[derive(Clone, Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
pub struct SelectGen<V: ValueLifetimeless> {
    /// The conditions, as given (strings or labels), and their values.
    pub branches: Vec<(V, V)>,
    pub no_match_error: String,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<SelectGen<To>> for SelectGen<From> {}

starlark_complex_value!(pub Select);

/// Values and selects concatenated with `+` or `|`.
#[derive(Clone, Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
pub struct SelectorListGen<V: ValueLifetimeless> {
    pub parts: Vec<V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<SelectorListGen<To>> for SelectorListGen<From> {}

starlark_complex_value!(pub SelectorList);

impl<'v, V: ValueLike<'v>> fmt::Display for SelectGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "select({{")?;
        for (i, (condition, value)) in self.branches.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}: {}", condition.to_value().to_repr(), value.to_value().to_repr())?;
        }
        write!(f, "}})")
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for SelectorListGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                write!(f, " + ")?;
            }
            write!(f, "{}", part.to_value().to_repr())?;
        }
        Ok(())
    }
}

/// The parts of `value`, if it's a select or a concatenation of them.
pub fn selector_parts<'v>(value: Value<'v>) -> Option<Vec<Value<'v>>> {
    if Select::from_value(value).is_some() {
        return Some(vec![value]);
    }
    SelectorList::from_value(value).map(|list| list.parts.clone())
}

fn concat<'v>(left: Value<'v>, right: Value<'v>, heap: Heap<'v>) -> Value<'v> {
    let mut parts = selector_parts(left).unwrap_or_else(|| vec![left]);
    parts.extend(selector_parts(right).unwrap_or_else(|| vec![right]));
    heap.alloc(SelectorList { parts })
}

#[starlark_value(type = "select")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SelectGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn add(&self, rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let me = heap.alloc(Select {
            branches: self.branches.iter().map(|(c, v)| (c.to_value(), v.to_value())).collect(),
            no_match_error: self.no_match_error.clone(),
        });
        Some(Ok(concat(me, rhs, heap)))
    }

    fn radd(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let me = heap.alloc(Select {
            branches: self.branches.iter().map(|(c, v)| (c.to_value(), v.to_value())).collect(),
            no_match_error: self.no_match_error.clone(),
        });
        Some(Ok(concat(lhs, me, heap)))
    }

    fn bit_or(&self, other: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let me = heap.alloc(Select {
            branches: self.branches.iter().map(|(c, v)| (c.to_value(), v.to_value())).collect(),
            no_match_error: self.no_match_error.clone(),
        });
        Ok(concat(me, other, heap))
    }
}

#[starlark_value(type = "select")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SelectorListGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn add(&self, rhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let me = heap.alloc(SelectorList {
            parts: self.parts.iter().map(|p| p.to_value()).collect(),
        });
        Some(Ok(concat(me, rhs, heap)))
    }

    fn radd(&self, lhs: Value<'v>, heap: Heap<'v>) -> Option<starlark::Result<Value<'v>>> {
        let me = heap.alloc(SelectorList {
            parts: self.parts.iter().map(|p| p.to_value()).collect(),
        });
        Some(Ok(concat(lhs, me, heap)))
    }

    fn bit_or(&self, other: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let me = heap.alloc(SelectorList {
            parts: self.parts.iter().map(|p| p.to_value()).collect(),
        });
        Ok(concat(me, other, heap))
    }
}

#[starlark_module]
pub(crate) fn select_function(builder: &mut GlobalsBuilder) {
    /// `select(conditions, no_match_error = "")`.
    fn select<'v>(
        #[starlark(require = pos)] x: Value<'v>,
        #[starlark(require = named, default = "")] no_match_error: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let Some(dict) = DictRef::from_value(x) else {
            anyhow::bail!("select() takes a dict, not {}", x.get_type());
        };
        if dict.len() == 0 {
            anyhow::bail!("select({{}}) with no conditions is not allowed");
        }
        let branches = dict.iter().collect();
        Ok(heap.alloc(Select {
            branches,
            no_match_error: no_match_error.to_string(),
        }))
    }
}
