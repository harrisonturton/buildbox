//! C and C++: toolchains, their features, `cc_common` and `CcInfo`, and the
//! rules that compile and link.
//!
//! Bazel 8 implements the C++ rules in Starlark on top of native APIs; bb
//! implements both the rules and the APIs other rules use natively, with
//! command lines from the toolchain's features as Bazel computes them.

pub mod api;
pub mod compile;
pub mod config;
pub mod features;
pub mod internal;
pub mod link;
pub mod rules;
pub mod solib;
pub mod values;
pub mod variables;

/// The kinds of rule implemented here in place of Bazel's builtins.
pub const NATIVE_RULES: &[&str] = &["cc_toolchain", "cc_toolchain_alias", "cc_library", "cc_binary", "cc_test"];

/// Add the C++ API: the providers to `.bzl` files, and the native API
/// `@_builtins`' Starlark `cc_common` is written against. Call before any
/// file is evaluated.
pub fn register() {
    bazel_starlark::register_bzl_extension(api::register);
    bazel_starlark::register_builtins_internal(internal::register_builtins_internal);
    bazel_starlark::provider::set_native_provider_name_fn(|value| starlark::values::ValueLike::downcast_ref::<values::NativeProvider>(value).map(|provider| provider.name));
}
