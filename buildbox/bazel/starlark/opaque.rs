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

    fn invoke(&self, _me: Value<'v>, _args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Ok(eval.heap().alloc(Opaque::new(&format!("{}()", self.type_name))))
    }
}

/// The builtin namespaces and providers `.bzl` files can name, stood in for
/// by opaque values while loading.
pub const BUILTIN_NAMESPACES: &[&str] = &[
    "platform_common",
    "config_common",
    "coverage_common",
    "testing",
    "apple_common",
    "cc_common",
    "java_common",
    "android_common",
    "py_common",
    "proto_common_do_not_use",
    "config",
    "proto",
    "DefaultInfo",
    "OutputGroupInfo",
    "RunEnvironmentInfo",
    "InstrumentedFilesInfo",
    "CcInfo",
    "CcToolchainConfigInfo",
    "DebugPackageInfo",
    "JavaInfo",
    "JavaPluginInfo",
    "ProguardSpecProvider",
    "PyInfo",
    "PyRuntimeInfo",
    "ProtoInfo",
    "PackageSpecificationInfo",
    "AnalysisTestResultInfo",
    "cc_proto_aspect",
    "java_proto_common",
    "Actions",
];
