//! Values that only need to exist, not yet do anything, while loading.

use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// A value of a Bazel API that loading doesn't look inside, such as an
/// aspect, a transition, or `cc_common`.
///
/// Its attributes are opaque values too, and calling it returns another,
/// so code that only passes these values around at the top level of a
/// `.bzl` file, or in the definitions of rules, loads. Using them in a
/// rule's implementation needs the real API.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Opaque {
    pub type_name: String,
}

starlark_simple_value!(Opaque);

impl Opaque {
    pub fn new(type_name: &str) -> Opaque {
        Opaque {
            type_name: type_name.to_string(),
        }
    }
}

impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.type_name)
    }
}

#[starlark_value(type = "opaque")]
impl<'v> StarlarkValue<'v> for Opaque {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(Opaque::new(&format!("{}.{attribute}", self.type_name))))
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }

    /// Opaque values are equal when they stand for the same API, as the
    /// real objects are.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<Opaque>().is_some_and(|other| other.type_name == self.type_name))
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.type_name.hash(hasher);
        Ok(())
    }

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        if self.type_name == "_builtins.toplevel.config_common.toolchain_type" {
            return builtins_toolchain_type(args, eval).map_err(starlark::Error::new_other);
        }
        if is_flag_getter(&self.type_name) {
            return Ok(Value::new_bool(false));
        }
        Ok(eval.heap().alloc(Opaque::new(&format!("{}()", self.type_name))))
    }

    fn iterate_collect(&self, _heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        Ok(Vec::new())
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(0)
    }

    fn is_in(&self, _other: Value<'v>) -> starlark::Result<bool> {
        Ok(false)
    }

    fn to_bool(&self) -> bool {
        true
    }
}

/// A namespace of the Bazel API with some real members, such as
/// `platform_common`. Members it doesn't have are opaque.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Namespace {
    pub name: String,
    #[allocative(skip)]
    pub members: Vec<(String, FrozenValue)>,
}

starlark_simple_value!(Namespace);

impl Namespace {
    pub fn new(name: &str, members: Vec<(String, FrozenValue)>) -> Namespace {
        Namespace {
            name: name.to_string(),
            members,
        }
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.name)
    }
}

#[starlark_value(type = "namespace")]
impl<'v> StarlarkValue<'v> for Namespace {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match self.members.iter().find(|(name, _)| name == attribute) {
            Some((_, value)) => Some(value.to_value()),
            None => Some(heap.alloc(Opaque::new(&format!("{}.{attribute}", self.name)))),
        }
    }

    fn has_attr(&self, _attribute: &str, _heap: Heap<'v>) -> bool {
        true
    }
}

/// Whether an API call reads an `--incompatible_*` or `--experimental_*`
/// flag. Such flags default to off, and code branches on them, so they
/// must be false rather than opaque.
fn is_flag_getter(type_name: &str) -> bool {
    let name = type_name.rsplit('.').next().unwrap_or(type_name);
    name.starts_with("incompatible_") || name.starts_with("experimental_")
}

/// `_builtins.toplevel.config_common.toolchain_type`, which the builtin
/// rules use to name the toolchains they need, as the real function.
fn builtins_toolchain_type<'v>(args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    let positions: Vec<Value<'v>> = args.positions(eval.heap()).map_err(|error| anyhow::anyhow!("{error}"))?.collect();
    let names = args.names_map().map_err(|error| anyhow::anyhow!("{error}"))?;
    let Some(name) = positions.first().copied() else {
        anyhow::bail!("toolchain_type() takes a label");
    };
    let mandatory = names
        .iter()
        .find(|(key, _)| key.as_str() == "mandatory")
        .map(|(_, value)| value.to_bool())
        .unwrap_or(true);
    let toolchain_type = crate::toolchain_type::toolchain_type(name, mandatory, eval)?;
    Ok(eval.heap().alloc(toolchain_type))
}

/// The builtin namespaces and providers Bazel 8 gives `.bzl` files, stood
/// in for by opaque values while loading. Those Bazel 8 moved out to rule
/// sets, such as `JavaInfo` and `PyInfo`, are deliberately missing, since
/// rule sets check for them to decide whether to define their own.
pub const BUILTIN_NAMESPACES: &[&str] = &[
    "coverage_common",
    "testing",
    "apple_common",
    "cc_common",
    "proto_common_do_not_use",
    "config",
    "proto",
    "CcInfo",
    "CcToolchainConfigInfo",
    "CcSharedLibraryInfo",
    "CcSharedLibraryHintInfo",
    "DebugPackageInfo",
    "PackageSpecificationInfo",
    "AnalysisTestResultInfo",
    "AnalysisFailureInfo",
    "Actions",
];

/// What `native.legacy_globals` holds in Bazel 8: the symbols Bazel moved
/// to rule sets, for rule sets that still need the originals.
pub const LEGACY_GLOBALS: &[&str] = &[
    "JavaInfo",
    "JavaPluginInfo",
    "ProtoInfo",
    "PyInfo",
    "PyRuntimeInfo",
    "PyCcLinkParamsProvider",
    "CcSharedLibraryInfo",
    "CcSharedLibraryHintInfo",
];
