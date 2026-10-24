//! `depset`: sets built by sharing other sets, for analysis.

use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::list::AllocList;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::collections::HashMap;
use std::fmt;

/// The order a depset's elements are listed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Allocative)]
pub enum Order {
    /// Left to right, children before their parents.
    Default,
    Postorder,
    /// Left to right, parents before their children.
    Preorder,
    /// Parents before children, in dependency order.
    Topological,
}

impl Order {
    fn parse(name: &str) -> anyhow::Result<Order> {
        Ok(match name {
            "default" | "stable" => Order::Default,
            "postorder" | "compile" => Order::Postorder,
            "preorder" | "naive_link" => Order::Preorder,
            "topological" | "link" => Order::Topological,
            other => anyhow::bail!("invalid depset order '{other}'"),
        })
    }

    fn compatible(self, other: Order) -> bool {
        self == other || self == Order::Default || other == Order::Default
    }
}

/// A depset: its direct elements and the depsets it includes, shared rather
/// than copied.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct DepsetGen<V: ValueLifetimeless> {
    pub direct: Vec<V>,
    pub transitive: Vec<V>,
    #[freeze(identity)]
    pub order: Order,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<DepsetGen<To>> for DepsetGen<From> {}

starlark_complex_value!(pub Depset);

impl<'v, V: ValueLike<'v>> fmt::Display for DepsetGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "depset([")?;
        for (i, value) in self.to_list().iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", value.to_repr())?;
        }
        write!(f, "])")
    }
}

impl<'v, V: ValueLike<'v>> DepsetGen<V> {
    /// Every element, once, in the depset's order.
    pub fn to_list(&self) -> Vec<Value<'v>> {
        let mut seen = Seen::default();
        let mut out = Vec::new();
        match self.order {
            Order::Default | Order::Postorder => self.postorder(&mut seen, &mut out),
            Order::Preorder => self.preorder(&mut seen, &mut out),
            Order::Topological => {
                self.reverse_postorder(&mut seen, &mut out);
                out.reverse();
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.direct.is_empty()
            && self
                .transitive
                .iter()
                .all(|child| Depset::from_value(child.to_value()).is_none_or(|child| child.is_empty()))
    }

    fn children(&self) -> impl Iterator<Item = &'v Depset<'v>> + '_ {
        self.transitive.iter().filter_map(|child| Depset::from_value(child.to_value()))
    }

    fn postorder(&self, seen: &mut Seen<'v>, out: &mut Vec<Value<'v>>) {
        for child in self.children() {
            child.postorder(seen, out);
        }
        for value in &self.direct {
            seen.add(value.to_value(), out);
        }
    }

    fn preorder(&self, seen: &mut Seen<'v>, out: &mut Vec<Value<'v>>) {
        for value in &self.direct {
            seen.add(value.to_value(), out);
        }
        for child in self.children() {
            child.preorder(seen, out);
        }
    }

    fn reverse_postorder(&self, seen: &mut Seen<'v>, out: &mut Vec<Value<'v>>) {
        for value in self.direct.iter().rev() {
            seen.add(value.to_value(), out);
        }
        for child in self.children().collect::<Vec<_>>().into_iter().rev() {
            child.reverse_postorder(seen, out);
        }
    }
}

/// The elements already listed: hashable ones by hash, compared by
/// Starlark equality; others, such as structs with list fields, by
/// identity.
#[derive(Default)]
struct Seen<'v> {
    by_hash: HashMap<u32, Vec<Value<'v>>>,
    by_identity: std::collections::HashSet<starlark::values::ValueIdentity<'v>>,
}

impl<'v> Seen<'v> {
    fn add(&mut self, value: Value<'v>, out: &mut Vec<Value<'v>>) {
        let Ok(hashed) = value.get_hashed() else {
            if self.by_identity.insert(value.identity()) {
                out.push(value);
            }
            return;
        };
        let hash = hashed.hash().get();
        let bucket = self.by_hash.entry(hash).or_default();
        if bucket.iter().any(|existing| existing.equals(value).unwrap_or(false)) {
            return;
        }
        bucket.push(value);
        out.push(value);
    }
}

#[starlark_value(type = "depset")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DepsetGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("depset", depset_methods);
        Some(METHODS.methods())
    }

    fn to_bool(&self) -> bool {
        !self.is_empty()
    }
}

#[starlark_module]
fn depset_methods(builder: &mut MethodsBuilder) {
    fn to_list<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let Some(depset) = Depset::from_value(this) else {
            anyhow::bail!("expected a depset");
        };
        Ok(heap.alloc(AllocList(depset.to_list())))
    }
}

#[starlark_module]
pub(crate) fn depset_function(builder: &mut GlobalsBuilder) {
    /// `depset(direct = None, order = "default", *, transitive = None)`.
    fn depset<'v>(
        direct: Option<Value<'v>>,
        #[starlark(default = "default")] order: &str,
        #[starlark(require = named)] transitive: Option<Value<'v>>,
    ) -> anyhow::Result<Depset<'v>> {
        let order = Order::parse(order)?;
        let list = |value: Option<Value<'v>>, what: &str| -> anyhow::Result<Vec<Value<'v>>> {
            match value.filter(|value| !value.is_none()) {
                Some(value) => crate::convert::items(value).ok_or_else(|| anyhow::anyhow!("depset: {what} must be a list")),
                None => Ok(Vec::new()),
            }
        };
        let direct = list(direct, "direct")?;
        let transitive = list(transitive, "transitive")?;
        for child in &transitive {
            let Some(child) = Depset::from_value(*child) else {
                anyhow::bail!("depset: transitive items must be depsets, not {}", child.get_type());
            };
            if !child.order.compatible(order) {
                anyhow::bail!("depset: order {:?} is incompatible with the order {:?} of a transitive depset", order, child.order);
            }
        }
        Ok(Depset { direct, transitive, order })
    }
}
