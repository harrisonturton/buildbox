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
    Freeze, FrozenHeap, FrozenHeapRef, FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};
use std::sync::{LazyLock, OnceLock};
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

impl<'v, V: ValueLike<'v>> ProviderGen<V> {
    /// Identifies the provider: its name and the file that defines it.
    /// Providers are looked up in targets by this key.
    pub fn key(&self) -> String {
        match self.definition {
            Some(file) => format!("{file}%{}", self.name()),
            None => self.name().to_string(),
        }
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

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(Provider::from_value(other).is_some_and(|other| other.key() == self.key()))
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.key().hash(hasher);
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

    /// Whether the instance has a field, as `"group" in OutputGroupInfo`
    /// asks.
    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.unpack_str().is_some_and(|name| self.fields.contains_key(name)))
    }
}

impl<'v> ProviderInstance<'v> {
    /// The key of the provider this is an instance of.
    pub fn provider_key(&self) -> Option<String> {
        Provider::from_value(self.provider).map(|provider| provider.key())
    }

    pub fn field(&self, name: &str) -> Option<Value<'v>> {
        self.fields.get(name).copied().filter(|value| !value.is_none())
    }
}

impl<'v, V: ValueLike<'v>> ProviderInstanceGen<V> {
    pub fn provider_name(&self) -> String {
        Provider::from_value(self.provider.to_value()).map(|provider| provider.name().to_string()).unwrap_or_default()
    }
}

/// The providers built into Bazel, with the fields each allows, or `None`
/// for any.
const BUILTIN_PROVIDERS: &[(&str, Option<&[&str]>)] = &[
    ("DefaultInfo", Some(&["files", "runfiles", "data_runfiles", "default_runfiles", "executable", "files_to_run"])),
    ("OutputGroupInfo", None),
    ("RunEnvironmentInfo", Some(&["environment", "inherited_environment"])),
    ("InstrumentedFilesInfo", None),
];

/// The providers of `platform_common`.
const PLATFORM_PROVIDERS: &[(&str, Option<&[&str]>)] = &[
    ("ToolchainInfo", None),
    ("TemplateVariableInfo", Some(&["variables"])),
    ("PlatformInfo", None),
    ("ConstraintValueInfo", None),
    ("ConstraintSettingInfo", None),
];

/// The builtin providers, allocated once, so every evaluation shares them.
struct BuiltinProviders {
    heap: FrozenHeapRef,
    toplevel: Vec<(&'static str, FrozenValue)>,
    platform_common: Vec<(String, FrozenValue)>,
}

static BUILTIN_PROVIDER_VALUES: LazyLock<BuiltinProviders> = LazyLock::new(|| {
    let heap = FrozenHeap::new();
    let alloc = |name: &str, fields: Option<&[&str]>| {
        let fields = fields.map(|fields| fields.iter().map(|field| field.to_string()).collect());
        heap.alloc_simple(FrozenProvider::builtin(name, fields))
    };
    let toplevel = BUILTIN_PROVIDERS.iter().map(|(name, fields)| (*name, alloc(name, *fields))).collect();
    let platform_common = PLATFORM_PROVIDERS.iter().map(|(name, fields)| (name.to_string(), alloc(name, *fields))).collect();
    BuiltinProviders {
        heap: heap.into_ref(),
        toplevel,
        platform_common,
    }
});

/// A builtin provider, such as `DefaultInfo` or `ToolchainInfo`, and the
/// heap it's in, which a heap that refers to it must keep alive.
pub fn builtin_provider(name: &str) -> Option<(FrozenValue, &'static FrozenHeapRef)> {
    let providers = &*BUILTIN_PROVIDER_VALUES;
    let toplevel = providers.toplevel.iter().map(|(name, value)| (*name, *value));
    let platform = providers.platform_common.iter().map(|(name, value)| (name.as_str(), *value));
    let mut all = toplevel.chain(platform);
    let (_, value) = all.find(|(candidate, _)| *candidate == name)?;
    Some((value, &providers.heap))
}

/// Define `DefaultInfo` and the other builtin providers, and the
/// `platform_common` namespace.
pub(crate) fn register_builtin_providers(builder: &mut GlobalsBuilder) {
    let providers = &*BUILTIN_PROVIDER_VALUES;
    builder.frozen_heap().add_reference(&providers.heap);
    for (name, value) in &providers.toplevel {
        builder.set(name, *value);
    }
    let namespace = builder.alloc(crate::opaque::Namespace::new("platform_common", providers.platform_common.clone()));
    builder.set("platform_common", namespace);
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
