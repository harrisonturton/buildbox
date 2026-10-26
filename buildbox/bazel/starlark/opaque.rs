//! Values that only need to exist, not yet do anything, while loading.

use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{FrozenValue, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value};
use std::cell::Cell;
use std::fmt;

thread_local! {
    static ANALYZING: Cell<bool> = const { Cell::new(false) };
}

/// While alive, opaque values on this thread can't be called, iterated or
/// searched, since a rule implementation using one needs the real API.
/// Without this, a rule would analyze without doing what it should.
pub struct Analyzing {
    previous: bool,
}

impl Analyzing {
    /// Make opaque values unusable on this thread until it's dropped.
    pub fn begin() -> Analyzing {
        Analyzing {
            previous: ANALYZING.replace(true),
        }
    }
}

impl Drop for Analyzing {
    fn drop(&mut self) {
        ANALYZING.set(self.previous);
    }
}

/// Whether an [`Analyzing`] is alive on this thread.
pub fn is_analyzing() -> bool {
    ANALYZING.get()
}

/// A value of a Bazel API that loading doesn't look inside, such as an
/// aspect, a transition, or `cc_common`.
///
/// Its attributes are opaque values too, and calling it returns another,
/// so code that only passes these values around at the top level of a
/// `.bzl` file, or in the definitions of rules, loads. A rule's
/// implementation can pass them around too, but fails if it calls,
/// iterates or searches one, or returns one as a provider, unless the value
/// is lenient.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct Opaque {
    pub type_name: String,
    /// Whether rule implementations can use it and the values derived from
    /// it as they can while loading.
    pub lenient: bool,
}

starlark_simple_value!(Opaque);

impl Opaque {
    pub fn new(type_name: &str) -> Opaque {
        Opaque {
            type_name: type_name.to_string(),
            lenient: false,
        }
    }

    /// An opaque value that rule implementations can use, for an API that
    /// rules use unconditionally but whose results only matter on platforms
    /// bb doesn't build for.
    pub fn lenient(type_name: &str) -> Opaque {
        Opaque {
            type_name: type_name.to_string(),
            lenient: true,
        }
    }

    /// An opaque value derived from this one, as lenient as it is.
    fn derived(&self, type_name: String) -> Opaque {
        Opaque {
            type_name,
            lenient: self.lenient,
        }
    }

    /// Fail if an opaque value can't stand in for its API here.
    fn check_usable(&self, api: &str) -> starlark::Result<()> {
        match is_analyzing() && !self.lenient {
            true => Err(unsupported(api)),
            false => Ok(()),
        }
    }
}

/// The error for using an API bb only has an opaque value for.
fn unsupported(api: &str) -> starlark::Error {
    starlark::Error::new_other(anyhow::anyhow!("'{api}' isn't supported by bb yet"))
}

impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.type_name)
    }
}

#[starlark_value(type = "opaque")]
impl<'v> StarlarkValue<'v> for Opaque {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        Some(heap.alloc(self.derived(format!("{}.{attribute}", self.type_name))))
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
        if self.type_name == "_builtins.internal.apple_common.dotted_version" {
            return dotted_version_call(args, eval).map_err(starlark::Error::new_other);
        }
        if is_flag_getter(&self.type_name) {
            return Ok(Value::new_bool(flag_default(&self.type_name)));
        }
        let called = format!("{}()", self.type_name);
        self.check_usable(&called)?;
        Ok(eval.heap().alloc(self.derived(called)))
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        Ok(heap.alloc(self.derived(format!("{}[{}]", self.type_name, index.to_repr()))))
    }

    fn iterate_collect(&self, _heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        self.check_usable(&self.type_name)?;
        Ok(Vec::new())
    }

    fn length(&self) -> starlark::Result<i32> {
        self.check_usable(&self.type_name)?;
        Ok(0)
    }

    fn is_in(&self, _other: Value<'v>) -> starlark::Result<bool> {
        self.check_usable(&self.type_name)?;
        Ok(false)
    }

    fn to_bool(&self) -> bool {
        true
    }
}

/// A namespace of the Bazel API with some real members, such as
/// `platform_common`. Members it doesn't have are opaque.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
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

/// The flags behind getters builtins call that Bazel 9 enables by
/// default. Every other `incompatible_` and `experimental_` flag is off.
const FLAGS_ON_BY_DEFAULT: &[&str] = &["incompatible_enable_proto_toolchain_resolution"];

/// The default value of the flag a getter such as
/// `proto_common_do_not_use.incompatible_enable_proto_toolchain_resolution`
/// returns.
fn flag_default(type_name: &str) -> bool {
    let name = type_name.rsplit('.').next().unwrap_or(type_name);
    FLAGS_ON_BY_DEFAULT.contains(&name)
}

/// `_builtins.internal.apple_common.dotted_version`, as the real function.
fn dotted_version_call<'v>(args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    let positions: Vec<Value<'v>> = args.positions(eval.heap()).map_err(|error| anyhow::anyhow!("{error}"))?.collect();
    let [version] = positions.as_slice() else {
        anyhow::bail!("dotted_version() takes one version");
    };
    crate::dotted_version::dotted_version(*version, eval.heap())
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

/// The builtin namespaces and providers Bazel gives `.bzl` files, stood
/// in for by opaque values while loading. Those Bazel moved out to rule
/// sets, such as `JavaInfo` and `PyInfo`, are deliberately missing, since
/// rule sets check for them to decide whether to define their own.
pub const BUILTIN_NAMESPACES: &[&str] = &[
    "coverage_common",
    "testing",
    "apple_common",
    "cc_common",
    "proto_common_do_not_use",
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

/// The globals `native.legacy_globals` holds in Bazel 9, for rule sets that
/// want Bazel's own: the C++ providers and the namespaces of rule APIs.
pub const LEGACY_GLOBALS: &[&str] = &[
    "CcInfo",
    "CcSharedLibraryHintInfo",
    "CcSharedLibraryInfo",
    "CcToolchainConfigInfo",
    "DebugPackageInfo",
    "apple_common",
    "cc_common",
    "java_common",
    "proto_common_do_not_use",
];

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::syntax::{AstModule, Dialect};

    fn eval(code: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            module.set("java_common", module.heap().alloc(Opaque::new("java_common")));
            let ast = AstModule::parse("test.bzl", code.to_string(), &Dialect::Standard).map_err(|error| error.to_string())?;
            let mut eval = Evaluator::new(&module);
            let value = eval.eval_module(ast, &Globals::standard()).map_err(|error| error.to_string())?;
            Ok(value.to_str())
        })
    }

    #[test]
    fn stand_in_for_apis_while_loading() {
        assert_eq!(eval("java_common.compile(1)").unwrap(), "<java_common.compile()>");
        assert_eq!(eval("[x for x in java_common.providers]").unwrap(), "[]");
        assert_eq!(eval("1 in java_common.providers").unwrap(), "False");
    }

    #[test]
    fn fail_when_rules_use_them() {
        let _analyzing = Analyzing::begin();
        for code in ["java_common.compile(1)", "[x for x in java_common.providers]", "len(java_common.providers)", "1 in java_common.providers"] {
            let error = eval(code).unwrap_err();
            assert!(error.contains("isn't supported by bb yet"), "{code}: {error}");
        }
        assert!(eval("java_common.compile(1)").unwrap_err().contains("'java_common.compile()'"));
        assert_eq!(eval("java_common.JavaInfo == java_common.JavaInfo").unwrap(), "True");
    }

    #[test]
    fn lenient_ones_stay_usable_when_rules_use_them() {
        let _analyzing = Analyzing::begin();
        let code = "xcode_config.minimum_os_for_platform_type(1).foo()";
        let result = Module::with_temp_heap(|module| {
            module.set("xcode_config", module.heap().alloc(Opaque::lenient("_xcode_config")));
            let ast = AstModule::parse("test.bzl", code.to_string(), &Dialect::Standard).map_err(|error| error.to_string())?;
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &Globals::standard()).map(|value| value.to_str()).map_err(|error| error.to_string())
        });
        assert_eq!(result.unwrap(), "<_xcode_config.minimum_os_for_platform_type().foo()>");
    }

    #[test]
    fn are_usable_again_once_analysis_ends() {
        drop(Analyzing::begin());
        assert!(eval("java_common.compile(1)").is_ok());
    }
}
