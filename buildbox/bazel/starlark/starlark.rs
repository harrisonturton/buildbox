//! Bazel's dialect of Starlark: the values and builtins BUILD and `.bzl`
//! files use while packages are loaded.

pub mod attr_module;
pub mod build_api;
pub mod bzl_api;
pub mod context;
pub mod convert;
pub mod depset;
pub mod eval;
pub mod instantiate;
pub mod label_value;
pub mod native_rules;
pub mod opaque;
pub mod package_builder;
pub mod provider;
pub mod repo_api;
pub mod rule_value;
pub mod select;
pub mod symbolic_macro;

pub use context::{EvalContext, FileKind, Globber};
pub use eval::{EvalError, Loads, eval_bzl, eval_build, loads, parse};
pub use package_builder::PackageBuilder;

use starlark::environment::{Globals, GlobalsBuilder, LibraryExtension};
use starlark::syntax::{Dialect, DialectTypes};
use std::sync::LazyLock;

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
    GlobalsBuilder::extended_by(&[
        LibraryExtension::StructType,
        LibraryExtension::Json,
        LibraryExtension::Print,
        LibraryExtension::Map,
        LibraryExtension::Filter,
        LibraryExtension::Partial,
        LibraryExtension::Debug,
        LibraryExtension::SetType,
    ])
}

/// The globals of BUILD files.
pub static BUILD_GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    let mut builder = standard()
        .with(build_api::build_functions)
        .with(select::select_function)
        .with(label_value::label_constructor);
    native_rules::register(&mut builder);
    builder.build()
});

/// The globals of `.bzl` files.
pub static BZL_GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
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
    for name in opaque::BUILTIN_NAMESPACES {
        let value = builder.alloc(opaque::Opaque::new(name));
        builder.set(name, value);
    }
    builder.namespace("native", |native| {
        build_api::build_functions(native);
        native_rules::register(native);
    });
    builder.build()
});
