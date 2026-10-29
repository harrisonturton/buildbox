//! A symbol `--incompatible_autoload_externally` names that couldn't be
//! loaded, because the build doesn't depend on the module defining it.

use allocative::Allocative;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, starlark_value};
use std::fmt;

/// Stands in for an autoloaded symbol whose module isn't a dependency,
/// failing as Bazel's does when it's called.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct AutoloadUnavailable {
    pub symbol: String,
    /// The `.bzl` file that defines it.
    pub label: String,
    /// The module of that file.
    pub module: String,
}

starlark_simple_value!(AutoloadUnavailable);

impl fmt::Display for AutoloadUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<function {}>", self.symbol)
    }
}

#[starlark_value(type = "function")]
impl<'v> StarlarkValue<'v> for AutoloadUnavailable {
    fn invoke(&self, _me: Value<'v>, _args: &Arguments<'v, '_>, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        Err(starlark::Error::new_other(anyhow::anyhow!(
            "Couldn't auto load '{}' from '{}'. Ensure that you have a 'bazel_dep(name = \"{}\", ...)' in your MODULE.bazel file or add an explicit load statement to your BUILD file.",
            self.symbol,
            self.label,
            self.module
        )))
    }
}
