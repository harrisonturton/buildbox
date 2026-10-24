//! `subrule()`: parts of a rule's implementation with dependencies of their
//! own, which the rules that use them declare in `subrules`.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use allocative::Allocative;
use bazel_label::Label;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::{
    Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, StringValue, Trace, Value, ValueLifetimeless, ValueLike,
    starlark_value,
};
use std::fmt;
use std::sync::OnceLock;

/// The value of one of a subrule's attributes in the rule calling it, given
/// the rule's `ctx`, the attribute's name in the rule, and its declaration.
pub type AttrValueFn = for<'v> fn(Value<'v>, &str, &Attribute<'v>, &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>>;

/// How analysis finds a subrule attribute's value; set by the analysis
/// layer, which knows what rule contexts hold.
static ATTR_VALUE: OnceLock<AttrValueFn> = OnceLock::new();

/// The name of the module variable that holds the `ctx` of the rule being
/// analysed, which subrules run with.
pub const RULE_CTX: &str = "$ctx";

/// Set how subrule attributes' values are found during analysis.
pub fn set_attr_value_fn(attr_value: AttrValueFn) {
    let _ = ATTR_VALUE.set(attr_value);
}

/// A subrule. It takes its name from the global it's assigned to, and its
/// attributes become hidden attributes of the rules that declare it.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct SubruleGen<V: ValueLifetimeless> {
    pub implementation: V,
    /// Its attributes, all private, as `attr.*()` values.
    pub attrs: Vec<(String, V)>,
    /// The toolchain types it needs, which the rules using it resolve.
    pub toolchains: Vec<V>,
    /// The subrules it calls.
    pub subrules: Vec<V>,
    /// The `.bzl` file that called `subrule()`.
    #[freeze(identity)]
    #[allocative(skip)]
    pub definition: Label,
    #[freeze(identity)]
    #[allocative(skip)]
    pub exported: OnceLock<String>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<SubruleGen<To>> for SubruleGen<From> {}

starlark_complex_value!(pub Subrule);

impl<'v, V: ValueLike<'v>> fmt::Display for SubruleGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exported.get() {
            Some(name) => write!(f, "<subrule {name}>"),
            None => write!(f, "<subrule>"),
        }
    }
}

impl<'v, V: ValueLike<'v>> SubruleGen<V> {
    /// The name the attribute `attr` has in the rules that declare this
    /// subrule, as Bazel names it: hidden, and unique to the subrule.
    pub fn rule_attr_name(&self, attr: &str) -> anyhow::Result<String> {
        let Some(name) = self.exported.get() else {
            anyhow::bail!("a subrule must be exported, assigned to a global in a .bzl file, before a rule uses it");
        };
        Ok(format!("${}%{name}%{attr}", self.definition))
    }

    /// The attributes of the rules that declare this subrule: its own and
    /// those of the subrules it calls, by their names in the rule.
    pub fn rule_attrs(&self) -> anyhow::Result<Vec<(String, Value<'v>)>> {
        let mut attrs = Vec::with_capacity(self.attrs.len());
        for (name, attribute) in &self.attrs {
            attrs.push((self.rule_attr_name(name)?, attribute.to_value()));
        }
        for subrule in &self.subrules {
            attrs.extend(of(subrule.to_value())?.rule_attrs()?);
        }
        Ok(attrs)
    }

    /// The toolchain types the rules that declare this subrule resolve for
    /// it and the subrules it calls.
    pub fn rule_toolchains(&self) -> anyhow::Result<Vec<Value<'v>>> {
        let mut toolchains: Vec<Value<'v>> = self.toolchains.iter().map(|toolchain| toolchain.to_value()).collect();
        for subrule in &self.subrules {
            toolchains.extend(of(subrule.to_value())?.rule_toolchains()?);
        }
        Ok(toolchains)
    }

    /// The keyword arguments the implementation gets for the attributes,
    /// as the rule being analysed, whose `ctx` is `ctx`, has them.
    fn attr_values(&self, ctx: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Vec<(String, Value<'v>)>> {
        let Some(attr_value) = ATTR_VALUE.get() else {
            anyhow::bail!("subrules can only be called while analysing a rule");
        };
        let mut values = Vec::with_capacity(self.attrs.len());
        for (name, attribute) in &self.attrs {
            let Some(attribute) = Attribute::from_value(attribute.to_value()) else {
                anyhow::bail!("attribute '{name}' of a subrule isn't an attr.*() value");
            };
            let value = attr_value(ctx, &self.rule_attr_name(name)?, attribute, eval)?;
            values.push((name.clone(), value));
        }
        Ok(values)
    }
}

/// The subrule `value` is.
fn of<'v>(value: Value<'v>) -> anyhow::Result<&'v Subrule<'v>> {
    Subrule::from_value(value).ok_or_else(|| anyhow::anyhow!("subrules must be subrule() values, not {}", value.get_type()))
}

/// The subrules a rule's or subrule's `subrules` lists.
pub fn subrules<'v>(list: Option<Value<'v>>) -> anyhow::Result<Vec<&'v Subrule<'v>>> {
    let Some(list) = list.filter(|list| !list.is_none()) else {
        return Ok(Vec::new());
    };
    let items = crate::convert::items(list).ok_or_else(|| anyhow::anyhow!("subrules must be a list"))?;
    items.into_iter().map(of).collect()
}

#[starlark_value(type = "subrule")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for SubruleGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn export_as(&self, variable_name: &str, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        let _ = self.exported.set(variable_name.to_string());
        Ok(())
    }

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let Some(ctx) = eval.module().get(RULE_CTX).filter(|ctx| !ctx.is_none()) else {
            return Err(starlark::Error::new_other(anyhow::anyhow!("subrules can only be called from a rule's implementation")));
        };
        let mut positional = vec![ctx];
        positional.extend(args.positions(eval.heap())?);
        let given = args.names_map()?;
        let attrs = self.attr_values(ctx, eval).map_err(starlark::Error::new_other)?;
        let mut named: Vec<(&str, Value<'v>)> = given.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        for (name, value) in &attrs {
            if given.iter().any(|(given, _)| given.as_str() == name) {
                return Err(starlark::Error::new_other(anyhow::anyhow!(
                    "got invalid named argument: '{name}' is an implicit dependency and cannot be overridden"
                )));
            }
            named.push((name.as_str(), *value));
        }
        eval.eval_function(self.implementation.to_value(), &positional, &named)
    }
}

/// The attributes `subrule(attrs = ...)` declares, which must be private.
fn declared_attrs<'v>(attrs: Option<Value<'v>>) -> anyhow::Result<Vec<(String, Value<'v>)>> {
    let Some(attrs) = attrs.filter(|attrs| !attrs.is_none()) else {
        return Ok(Vec::new());
    };
    let Some(dict) = DictRef::from_value(attrs) else {
        anyhow::bail!("subrule(): attrs must be a dict");
    };
    let mut declared = Vec::with_capacity(dict.len());
    for (name, attribute) in dict.iter() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("subrule(): attribute names must be strings");
        };
        if !name.starts_with('_') {
            anyhow::bail!("illegal attribute name '{name}': subrules may only define private attributes (whose names begin with '_').");
        }
        declared.push((name.to_string(), attribute));
    }
    Ok(declared)
}

#[starlark_module]
pub(crate) fn subrule_function(builder: &mut GlobalsBuilder) {
    /// `subrule(implementation, attrs = {}, toolchains = [], fragments = [],
    /// subrules = [])`.
    fn subrule<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] subrules: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Subrule<'v>> {
        let _ = (fragments, kwargs);
        let context = EvalContext::get(eval)?;
        let toolchains = toolchains.filter(|toolchains| !toolchains.is_none()).map(crate::convert::items);
        let toolchains = toolchains.map(|items| items.ok_or_else(|| anyhow::anyhow!("subrule(): toolchains must be a list"))).transpose()?;
        let subrules = subrules.filter(|subrules| !subrules.is_none()).map(crate::convert::items);
        let subrules = subrules.map(|items| items.ok_or_else(|| anyhow::anyhow!("subrule(): subrules must be a list"))).transpose()?;
        Ok(SubruleGen {
            implementation,
            attrs: declared_attrs(attrs)?,
            toolchains: toolchains.unwrap_or_default(),
            subrules: subrules.unwrap_or_default(),
            definition: context.caller_file(eval),
            exported: OnceLock::new(),
        })
    }
}
