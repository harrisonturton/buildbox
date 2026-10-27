//! Java: the native API rules_java's Starlark rules are written against,
//! which compiles with the toolchain's JavaBuilder and Turbine as Bazel's
//! does.

mod api;
mod compile;
mod toolchain;

/// Add `java_common_internal_do_not_use` to `_builtins.internal`, which
/// `@_builtins`' `java_common` gives rules_java. Call before any file is
/// evaluated.
pub fn register() {
    bazel_starlark::register_builtins_internal(api::register_builtins_internal);
}
