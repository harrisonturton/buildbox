//! The Starlark values of the C++ API: `CcInfo` and the contexts in it,
//! toolchains, feature configurations and build variables.

use super::config::CcToolchainConfig;
use super::features::{FeatureConfiguration, Variables};
use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::eval::{Arguments, Evaluator};
use starlark::pagable::{StarlarkDeserialize, StarlarkDeserializeContext, StarlarkSerialize, StarlarkSerializeContext};
use starlark::starlark_complex_value;
use starlark::starlark_simple_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;
use std::sync::Arc;

/// A value of the C++ API with named fields, such as a `CcInfo`, a
/// `LinkerInput` or a `LibraryToLink`. `kind` names its type.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct CcStructGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_static_str", deserialize_with = "bazel_starlark::paging::deserialize_static_str")]
    pub kind: &'static str,
    pub fields: SmallMap<String, V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<CcStructGen<To>> for CcStructGen<From> {}

starlark_complex_value!(pub CcStruct);

impl<'v> CcStruct<'v> {
    pub fn new(kind: &'static str, fields: Vec<(&str, Value<'v>)>) -> CcStruct<'v> {
        CcStruct {
            kind,
            fields: fields.into_iter().map(|(name, value)| (name.to_string(), value)).collect(),
        }
    }

    /// A field, if the value is a struct of `kind`.
    pub fn field_of(value: Value<'v>, kind: &str, field: &str) -> Option<Value<'v>> {
        let value = CcStruct::from_value(value)?;
        if value.kind != kind {
            return None;
        }
        value.fields.get(field).copied()
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for CcStructGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.kind)
    }
}

#[starlark_value(type = "CcStruct")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CcStructGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields.get(attribute).map(|value| value.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.keys().cloned().collect()
    }

    /// Equal only to themselves, as Bazel's are; depsets of them are
    /// deduplicated by identity.
    fn equals(&self, _other: Value<'v>) -> starlark::Result<bool> {
        Ok(false)
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.kind.hash(hasher);
        if let Some(owner) = self.fields.get("owner") {
            owner.to_value().to_str().hash(hasher);
        }
        Ok(())
    }
}

/// Builds an instance of a [`NativeProvider`] from a call's arguments.
pub type Constructor = for<'v> fn(&Arguments<'v, '_>, &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>>;

/// A provider built into Bazel whose instances are values of their own,
/// such as `CcInfo`, rather than plain provider instances.
#[derive(ProvidesStaticType, NoSerialize, Allocative)]
pub struct NativeProvider {
    pub name: &'static str,
    #[allocative(skip)]
    pub constructor: Option<Constructor>,
}

starlark_simple_value!(NativeProvider);

/// A native provider is serialized as its name; its constructor is found
/// again by name.
impl StarlarkSerialize for NativeProvider {
    fn starlark_serialize(&self, ctx: &mut dyn StarlarkSerializeContext) -> starlark::Result<()> {
        bazel_starlark::paging::serialize_static_str(&self.name, ctx)
    }
}

impl StarlarkDeserialize for NativeProvider {
    fn starlark_deserialize(ctx: &mut dyn StarlarkDeserializeContext<'_>) -> starlark::Result<NativeProvider> {
        let name = bazel_starlark::paging::deserialize_static_str(ctx)?;
        Ok(NativeProvider {
            name,
            constructor: super::api::native_constructor(name),
        })
    }
}

impl fmt::Debug for NativeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NativeProvider({})", self.name)
    }
}

impl fmt::Display for NativeProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<provider {}>", self.name)
    }
}

#[starlark_value(type = "Provider")]
impl<'v> StarlarkValue<'v> for NativeProvider {
    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let Some(constructor) = self.constructor else {
            return Err(starlark::Error::new_other(anyhow::anyhow!("{} can't be constructed", self.name)));
        };
        constructor(args, eval).map_err(starlark::Error::new_other)
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<NativeProvider>().is_some_and(|other| other.name == self.name))
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.name.hash(hasher);
        Ok(())
    }
}

/// A C++ toolchain, as `CcToolchainInfo` gives it: its configuration, and
/// the files its tools need.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct CcToolchainGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    pub config: Arc<CcToolchainConfig>,
    /// Depsets of files, such as `all_files` and `compiler_files`, by name.
    pub files: SmallMap<String, V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<CcToolchainGen<To>> for CcToolchainGen<From> {}

starlark_complex_value!(pub CcToolchain);

impl<'v, V: ValueLike<'v>> fmt::Display for CcToolchainGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcToolchainInfo {}>", self.config.toolchain_identifier)
    }
}

impl<'v, V: ValueLike<'v>> CcToolchainGen<V> {
    pub fn file_set(&self, name: &str) -> Option<Value<'v>> {
        self.files.get(name).map(|value| value.to_value())
    }
}

#[starlark_value(type = "CcToolchainInfo")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CcToolchainGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static starlark::environment::Methods> {
        static METHODS: starlark::environment::MethodsStatic = starlark::environment::MethodsStatic::new("CcToolchainInfo", super::api::toolchain_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        if let Some(files) = self.files.get(attribute).or_else(|| self.files.get(attribute.trim_start_matches('_'))) {
            return Some(files.to_value());
        }
        super::api::toolchain_attr(&self.config, attribute, heap)
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        names.extend(super::api::TOOLCHAIN_ATTRS.iter().map(|name| name.to_string()));
        names
    }
}

/// A `FeatureConfiguration`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct FeatureConfigurationValue(#[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")] #[allocative(skip)] pub Arc<FeatureConfiguration>);

starlark_simple_value!(FeatureConfigurationValue);

impl fmt::Display for FeatureConfigurationValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<FeatureConfiguration>")
    }
}

#[starlark_value(type = "FeatureConfiguration")]
impl<'v> StarlarkValue<'v> for FeatureConfigurationValue {}

/// Build variables, as `cc_common.create_compile_variables` makes them.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct VariablesValue(#[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")] #[allocative(skip)] pub Arc<Variables>);

starlark_simple_value!(VariablesValue);

impl fmt::Display for VariablesValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcToolchainVariables>")
    }
}

#[starlark_value(type = "CcToolchainVariables")]
impl<'v> StarlarkValue<'v> for VariablesValue {}

/// A toolchain configuration, as `CcToolchainConfigInfo` gives it.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct ToolchainConfigValue(#[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")] #[allocative(skip)] pub Arc<CcToolchainConfig>);

starlark_simple_value!(ToolchainConfigValue);

impl fmt::Display for ToolchainConfigValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<CcToolchainConfigInfo {}>", self.0.toolchain_identifier)
    }
}

#[starlark_value(type = "CcToolchainConfigInfo")]
impl<'v> StarlarkValue<'v> for ToolchainConfigValue {}
