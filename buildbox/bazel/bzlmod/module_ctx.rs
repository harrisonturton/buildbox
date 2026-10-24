//! `module_ctx`, and the module and tag objects it gives extensions.

use crate::io::{HasIo, IoState};
use allocative::Allocative;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// `module_ctx`: what a module extension's implementation is given.
#[derive(Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ModuleCtxGen<V: ValueLifetimeless> {
    /// Every module that uses the extension, root first.
    pub modules: V,
    #[freeze(identity)]
    #[allocative(skip)]
    pub io: IoState,
    pub root_module_has_non_dev_dependency: bool,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ModuleCtxGen<To>> for ModuleCtxGen<From> {}

starlark_complex_value!(pub ModuleCtx);

impl<V: ValueLifetimeless> fmt::Debug for ModuleCtxGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "module_ctx")
    }
}

impl<V: ValueLifetimeless> fmt::Display for ModuleCtxGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module_ctx>")
    }
}

impl<V: ValueLifetimeless> HasIo for ModuleCtxGen<V> {
    fn io(&self) -> &IoState {
        &self.io
    }
}

#[starlark_value(type = "module_ctx")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ModuleCtxGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("module_ctx", |builder| {
            crate::io::io_methods(builder);
            module_methods(builder);
        });
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "modules" => Some(self.modules.to_value()),
            "root_module_has_non_dev_dependency" => Some(heap.alloc(self.root_module_has_non_dev_dependency)),
            _ => None,
        }
    }
}

#[starlark_module]
fn module_methods(builder: &mut MethodsBuilder) {
    /// Whether a tag came from a `dev_dependency = True` use.
    fn is_dev_dependency<'v>(this: Value<'v>, #[starlark(require = pos)] tag: Value<'v>) -> anyhow::Result<bool> {
        let _ = this;
        match Tag::from_value(tag) {
            Some(tag) => Ok(tag.dev),
            None => anyhow::bail!("is_dev_dependency() takes a tag"),
        }
    }

    /// What the extension tells Bazel about the repositories it generates.
    /// Accepted and ignored.
    fn extension_metadata<'v>(
        this: Value<'v>,
        #[starlark(kwargs)] kwargs: SmallMap<String, Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, kwargs);
        Ok(NoneType)
    }
}

/// A tag, with its attributes converted by its tag class.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct TagGen<V: ValueLifetimeless> {
    pub attrs: SmallMap<String, V>,
    pub dev: bool,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<TagGen<To>> for TagGen<From> {}

starlark_complex_value!(pub Tag);

impl<'v, V: ValueLike<'v>> fmt::Display for TagGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tag(")?;
        for (i, (name, value)) in self.attrs.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{name} = {}", value.to_value().to_repr())?;
        }
        write!(f, ")")
    }
}

#[starlark_value(type = "tag")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.attrs.get(attribute).map(|value| value.to_value())
    }

    fn has_attr(&self, attribute: &str, _heap: Heap<'v>) -> bool {
        self.attrs.contains_key(attribute)
    }

    fn dir_attr(&self) -> Vec<String> {
        self.attrs.keys().cloned().collect()
    }
}
