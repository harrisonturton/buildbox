//! `depset`: sets built by sharing other sets, for analysis.

use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::list::AllocList;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt;

/// The order a depset's elements are listed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Allocative, skycodec::Encode, skycodec::Decode)]
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
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct DepsetGen<V: ValueLifetimeless> {
    pub direct: Vec<V>,
    pub transitive: Vec<V>,
    #[freeze(identity)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
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
    /// Every element, once, in the depset's order, as Bazel lists them: a
    /// depset's elements and the depsets it includes are walked in its
    /// order, each included depset where it's first met, and the list of
    /// a topological depset reversed. The walk keeps its own stack, so deep
    /// depsets can't exhaust the thread's.
    pub fn to_list(&self) -> Vec<Value<'v>> {
        let mut seen = Seen::default();
        let mut out = Vec::new();
        let mut stack = vec![steps(self).into_iter()];
        while let Some(top) = stack.last_mut() {
            match top.next() {
                None => {
                    stack.pop();
                }
                Some(Step::Element(value)) => seen.add(value, &mut out),
                Some(Step::Child(value, child)) => {
                    if seen.visited.insert(value.identity()) {
                        stack.push(steps(child).into_iter());
                    }
                }
            }
        }
        if self.order == Order::Topological {
            out.reverse();
        }
        out
    }

    /// Whether the depset and those it includes have no elements.
    pub fn is_empty(&self) -> bool {
        if !self.direct.is_empty() {
            return false;
        }
        let mut visited: FxHashSet<starlark::values::ValueIdentity<'v>> = FxHashSet::default();
        let mut pending: Vec<Value<'v>> = self.transitive.iter().map(|child| child.to_value()).collect();
        while let Some(child) = pending.pop() {
            let Some(depset) = Depset::from_value(child) else {
                continue;
            };
            if !visited.insert(child.identity()) {
                continue;
            }
            if !depset.direct.is_empty() {
                return false;
            }
            pending.extend(depset.transitive.iter().copied());
        }
        true
    }
}

/// One step of walking a depset: an element, or a depset it includes.
enum Step<'v> {
    Element(Value<'v>),
    Child(Value<'v>, &'v Depset<'v>),
}

/// The steps of walking a depset's own elements and the depsets it
/// includes, in its order: postorder walks the depsets first, preorder
/// the elements first, and topological, whose list is reversed at the
/// end, the depsets first, each part reversed.
fn steps<'v, V: ValueLike<'v>>(set: &DepsetGen<V>) -> Vec<Step<'v>> {
    let direct = set.direct.iter().map(|value| Step::Element(value.to_value()));
    let children = set.transitive.iter().filter_map(|child| {
        let child = child.to_value();
        Depset::from_value(child).map(|depset| Step::Child(child, depset))
    });
    match set.order {
        Order::Default | Order::Postorder => children.chain(direct).collect(),
        Order::Preorder => direct.chain(children).collect(),
        Order::Topological => {
            let mut steps: Vec<Step<'v>> = children.collect();
            steps.reverse();
            steps.extend(direct.rev());
            steps
        }
    }
}

/// The elements already listed: hashable ones by hash, compared by
/// Starlark equality; others, such as structs with list fields, by
/// identity.
#[derive(Default)]
struct Seen<'v> {
    by_hash: FxHashMap<u32, Vec<Value<'v>>>,
    by_identity: FxHashSet<starlark::values::ValueIdentity<'v>>,
    /// The depsets already traversed.
    visited: FxHashSet<starlark::values::ValueIdentity<'v>>,
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

#[cfg(test)]
mod tests {
    use starlark::environment::{GlobalsBuilder, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    /// The elements of the depset `source` evaluates to, as a list's repr.
    fn listed(source: &str) -> String {
        let globals = GlobalsBuilder::standard().with(super::depset_function).build();
        let ast = AstModule::parse("test.bzl", source.to_string(), &Dialect::Extended).unwrap();
        Module::with_temp_heap(|module| {
            let mut eval = Evaluator::new(&module);
            let value = eval.eval_module(ast, &globals).unwrap();
            value.to_repr()
        })
    }

    const DIAMOND: &str = r#"
def create(order):
    a = depset(["a"], order = order)
    b = depset(["b"], transitive = [a], order = order)
    c = depset(["c"], transitive = [a], order = order)
    return depset(["d"], transitive = [b, c], order = order)
"#;

    #[test]
    fn diamonds_list_in_bazels_orders() {
        assert_eq!(listed(&format!("{DIAMOND}\ncreate('postorder').to_list()")), r#"["a", "b", "c", "d"]"#);
        assert_eq!(listed(&format!("{DIAMOND}\ncreate('preorder').to_list()")), r#"["d", "b", "a", "c"]"#);
        assert_eq!(listed(&format!("{DIAMOND}\ncreate('topological').to_list()")), r#"["d", "b", "c", "a"]"#);
    }

    #[test]
    fn an_included_depset_is_listed_where_it_is_first_met() {
        let source = r#"
inner = depset(["x"])
outer = depset(["y"], transitive = [inner])
depset(["z"], transitive = [outer, inner]).to_list()
"#;
        assert_eq!(listed(source), r#"["x", "y", "z"]"#);
    }

    #[test]
    fn deep_depsets_list_without_recursion() {
        let source = r#"
def deep():
    d = depset([0])
    for i in range(1, 100000):
        d = depset([i], transitive = [d])
    return len(d.to_list())
deep()
"#;
        assert_eq!(listed(source), "100000");
    }
}
