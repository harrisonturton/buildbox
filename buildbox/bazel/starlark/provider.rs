//! `provider()`, and the provider instances rules return.

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
use starlark::values::tuple::AllocTuple;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};
use std::sync::OnceLock;
use std::fmt;

/// A provider defined with `provider()`, or built in. Calling it makes an
/// instance; it takes its name from the global it's assigned to.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ProviderGen<V: ValueLifetimeless> {
    /// The allowed fields, if restricted.
    pub fields: Option<Vec<String>>,
    pub init: Option<V>,
    /// Calling the provider runs `init`; calling the raw constructor doesn't.
    pub raw: bool,
    #[freeze(identity)]
    #[allocative(skip)]
    pub definition: Option<Label>,
    #[freeze(identity)]
    #[allocative(skip)]
    pub name: OnceLock<String>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ProviderGen<To>> for ProviderGen<From> {}

starlark_complex_value!(pub Provider);

impl<'v, V: ValueLike<'v>> ProviderGen<V> {
    pub fn name(&self) -> &str {
        self.name.get().map(String::as_str).unwrap_or("<unexported provider>")
    }

    /// A provider built into Bazel, such as `DefaultInfo`.
    pub fn builtin(name: &str, fields: Option<Vec<String>>) -> ProviderGen<V> {
        let provider = ProviderGen {
            fields,
            init: None,
            raw: false,
            definition: None,
            name: OnceLock::new(),
        };
        let _ = provider.name.set(name.to_string());
        provider
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for ProviderGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<provider {}>", self.name())
    }
}

#[starlark_value(type = "Provider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn export_as(&self, variable_name: &str, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        let _ = self.name.set(variable_name.to_string());
        Ok(())
    }

    fn invoke(&self, me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        if let (Some(init), false) = (self.init, self.raw) {
            let result = eval.eval_function(init.to_value(), &args.positions(eval.heap())?.collect::<Vec<_>>(), &named(args)?)?;
            let Some(fields) = DictRef::from_value(result) else {
                return Err(starlark::Error::new_other(anyhow::anyhow!("{}: init must return a dict", self.name())));
            };
            let fields: SmallMap<String, Value<'v>> = fields.iter().filter_map(|(k, v)| Some((k.unpack_str()?.to_string(), v))).collect();
            return Ok(eval.heap().alloc(ProviderInstance { provider: me, fields }));
        }
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let mut fields = SmallMap::with_capacity(names.len());
        for (name, value) in names.iter() {
            if let Some(allowed) = &self.fields {
                if !allowed.iter().any(|field| field == name.as_str()) {
                    return Err(starlark::Error::new_other(anyhow::anyhow!(
                        "{}: unexpected field '{}'",
                        self.name(),
                        name.as_str()
                    )));
                }
            }
            fields.insert(name.as_str().to_string(), *value);
        }
        Ok(eval.heap().alloc(ProviderInstance { provider: me, fields }))
    }
}

fn named<'v>(args: &Arguments<'v, '_>) -> starlark::Result<Vec<(&'v str, Value<'v>)>> {
    let names = args.names_map()?;
    Ok(names.iter().map(|(name, value)| (name.as_str(), *value)).collect())
}

/// An instance of a provider: its fields.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ProviderInstanceGen<V: ValueLifetimeless> {
    pub provider: V,
    pub fields: SmallMap<String, V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ProviderInstanceGen<To>> for ProviderInstanceGen<From> {}

starlark_complex_value!(pub ProviderInstance);

impl<'v, V: ValueLike<'v>> fmt::Display for ProviderInstanceGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = Provider::from_value(self.provider.to_value()).map(|p| p.name().to_string()).unwrap_or_default();
        write!(f, "struct(")?;
        for (i, (field, value)) in self.fields.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{field} = {}", value.to_value().to_repr())?;
        }
        write!(f, ") of {name}")
    }
}

#[starlark_value(type = "struct")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderInstanceGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields.get(attribute).map(|value| value.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.keys().cloned().collect()
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        self.fields.contains_key(attribute)
    }
}

#[starlark_module]
pub(crate) fn provider_function(builder: &mut GlobalsBuilder) {
    /// `provider(doc = "", fields = None, init = None)`.
    fn provider<'v>(
        #[starlark(default = "")] doc: &str,
        #[starlark(require = named)] fields: Option<Value<'v>>,
        #[starlark(require = named)] init: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = doc;
        let context = EvalContext::get(eval)?;
        let fields = match fields {
            None => None,
            Some(fields) if fields.is_none() => None,
            Some(fields) => match DictRef::from_value(fields) {
                Some(dict) => Some(dict.keys().filter_map(|k| k.unpack_str().map(str::to_string)).collect()),
                None => crate::convert::strings(fields),
            },
        };
        let init = init.filter(|init| !init.is_none());
        let provider = Provider {
            fields: fields.clone(),
            init,
            raw: false,
            definition: Some(context.caller_file(eval)),
            name: OnceLock::new(),
        };
        let heap = eval.heap();
        let provider = heap.alloc(provider);
        if init.is_none() {
            return Ok(provider);
        }
        let raw = heap.alloc(Provider {
            fields,
            init: None,
            raw: true,
            definition: Some(context.caller_file(eval)),
            name: OnceLock::new(),
        });
        Ok(heap.alloc(AllocTuple([provider, raw])))
    }
}
