//! Bazel's dialect of Starlark: the values and builtins BUILD and `.bzl`
//! files use while packages are loaded.

pub mod attr_module;
pub mod autoload;
pub mod build_api;
pub mod bzl_api;
pub mod context;
pub mod convert;
pub mod depset;
pub mod eval;
pub mod instantiate;
pub mod json_module;
pub mod label_value;
pub mod native_rules;
pub mod opaque;
pub mod package_builder;
pub mod provider;
pub mod repo_api;
pub mod rule_value;
pub mod select;
pub mod symbolic_macro;
pub mod toolchain_type;

pub use context::{EvalContext, FileKind, Globber};
pub use eval::{EvalError, Loads, eval_bzl, eval_build, loaded_names, loads, parse};
pub use package_builder::PackageBuilder;

use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::values::{FrozenHeapRef, FrozenValue};
use starlark::syntax::{Dialect, DialectTypes};
use std::sync::LazyLock;

/// The Bazel version these builtins behave like, which `native.bazel_version`
/// and `bb version` report.
pub const BAZEL_VERSION: &str = "8.8.1";

/// The dialect BUILD and `.bzl` files are parsed with.
pub fn dialect() -> Dialect {
    Dialect {
        enable_def: true,
        enable_lambda: true,
        enable_load: true,
        enable_keyword_only_arguments: true,
        enable_positional_only_arguments: true,
        enable_types: DialectTypes::ParseOnly,
        enable_f_strings: false,
        ..Dialect::Standard
    }
}

fn standard() -> GlobalsBuilder {
    let mut builder = GlobalsBuilder::extended_by(&[
        LibraryExtension::StructType,
        LibraryExtension::Print,
        LibraryExtension::Map,
        LibraryExtension::Filter,
        LibraryExtension::Partial,
        LibraryExtension::Debug,
        LibraryExtension::SetType,
    ]);
    json_module::register(&mut builder);
    builder
}

/// Builtins defined in Starlark, as Bazel's `@_builtins` defines them,
/// added to the globals: rules for BUILD files and `native`, and values
/// for `.bzl` files.
pub struct StarlarkBuiltins<'a> {
    pub rules: Vec<(String, FrozenValue)>,
    pub toplevels: Vec<(String, FrozenValue)>,
    /// The heap the values are in, which the globals keep alive.
    pub heap: &'a FrozenHeapRef,
}

/// Add `native.bazel_version` and `native.legacy_globals`.
fn native_values(native: &mut GlobalsBuilder) {
    native.set("bazel_version", BAZEL_VERSION);
    let legacy: Vec<(&str, opaque::Opaque)> = opaque::LEGACY_GLOBALS.iter().map(|name| (*name, opaque::Opaque::new(name))).collect();
    let legacy = native.alloc(starlark::values::structs::AllocStruct(legacy));
    native.set("legacy_globals", legacy);
}

fn build_globals_builder() -> GlobalsBuilder {
    let mut builder = standard()
        .with(build_api::build_functions)
        .with(select::select_function)
        .with(label_value::label_constructor)
        .with(depset::depset_function);
    native_rules::register(&mut builder);
    builder
}

fn bzl_globals_builder() -> GlobalsBuilder {
    let mut builder = standard()
        .with(select::select_function)
        .with(label_value::label_constructor)
        .with(rule_value::rule_function)
        .with(provider::provider_function)
        .with(bzl_api::bzl_functions)
        .with(bzl_api::macro_function)
        .with(depset::depset_function)
        .with(repo_api::repo_functions);
    attr_module::register(&mut builder);
    toolchain_type::register(&mut builder);
    for name in opaque::BUILTIN_NAMESPACES {
        let value = builder.alloc(opaque::Opaque::new(name));
        builder.set(name, value);
    }
    provider::register_builtin_providers(&mut builder);
    builder
}

/// The globals of BUILD files, before builtins.
pub static BUILD_GLOBALS: LazyLock<Globals> = LazyLock::new(|| build_globals_builder().build());

/// The globals of `.bzl` files, before builtins. `@_builtins` itself is
/// evaluated with these and a stand-in `_builtins` object.
pub static BZL_GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    let mut builder = bzl_globals_builder();
    builder.namespace("native", |native| {
        build_api::build_functions(native);
        native_rules::register(native);
        native_values(native);
    });
    let value = builder.alloc(opaque::Opaque::new("_builtins"));
    builder.set("_builtins", value);
    builder.build()
});

/// The globals of BUILD files, with builtin rules.
pub fn build_globals(builtins: &StarlarkBuiltins<'_>) -> Globals {
    let mut builder = build_globals_builder();
    builder.frozen_heap().add_reference(builtins.heap);
    for (name, value) in &builtins.rules {
        builder.set(name, *value);
    }
    builder.build()
}

/// The globals of `.bzl` files, with builtin rules and values.
pub fn bzl_globals(builtins: &StarlarkBuiltins<'_>) -> Globals {
    let mut builder = bzl_globals_builder();
    builder.frozen_heap().add_reference(builtins.heap);
    for (name, value) in &builtins.toplevels {
        builder.set(name, *value);
    }
    builder.namespace("native", |native| {
        build_api::build_functions(native);
        native_rules::register(native);
        native_values(native);
        native.frozen_heap().add_reference(builtins.heap);
        for (name, value) in &builtins.rules {
            native.set(name, *value);
        }
    });
    builder.build()
}
