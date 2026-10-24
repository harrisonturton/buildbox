//! Values that only need to exist, not yet do anything, while loading.

use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, starlark_value};
use std::fmt;

/// A value of a Bazel API that loading doesn't look inside, such as an
/// aspect, a transition, or `platform_common.ToolchainInfo`.
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

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        if self.type_name == "_builtins.toplevel.config_common.toolchain_type" {
            return builtins_toolchain_type(args, eval).map_err(starlark::Error::new_other);
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
    "platform_common",
    "coverage_common",
    "testing",
    "apple_common",
    "cc_common",
    "proto_common_do_not_use",
    "config",
    "proto",
    "DefaultInfo",
    "OutputGroupInfo",
    "RunEnvironmentInfo",
    "InstrumentedFilesInfo",
    "CcInfo",
    "CcToolchainConfigInfo",
    "CcSharedLibraryInfo",
    "CcSharedLibraryHintInfo",
    "DebugPackageInfo",
    "PackageSpecificationInfo",
    "AnalysisTestResultInfo",
    "Actions",
];
