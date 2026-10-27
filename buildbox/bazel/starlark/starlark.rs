//! Bazel's dialect of Starlark: the values and builtins BUILD and `.bzl`
//! files use while packages are loaded.

pub mod aspect;
pub mod attr_module;
pub mod build_api;
pub mod bzl_api;
pub mod config_module;
pub mod context;
pub mod convert;
pub mod depset;
pub mod dotted_version;
pub mod eval;
pub mod fields;
mod hash;
pub mod instantiate;
pub mod json_module;
pub mod label_value;
pub mod native_rules;
pub mod opaque;
pub mod package_builder;
pub mod paging;
pub mod print;
pub mod provider;
pub mod repo_api;
pub mod reported;
pub mod rule_value;
pub mod select;
pub mod subrule;
pub mod symbolic_macro;
pub mod toolchain_type;
pub mod transition;

pub use context::{EvalContext, FileKind, Globber};
pub use eval::{EvalError, Loads, eval_bzl, eval_build, heap_name, load_positions, loads, parse, parse_reporting_all};
pub use package_builder::PackageBuilder;

use starlark::environment::{GlobalFrozenHeapName, Globals, GlobalsBuilder, LibraryExtension};
use starlark::values::{FrozenHeapRef, FrozenValue};
use starlark::syntax::{Dialect, DialectTypes};
use std::sync::LazyLock;

/// The Bazel version bb behaves like: which `native.bazel_version`
/// reports, and whose `@bazel_tools` and `@_builtins` it uses.
pub const BAZEL_VERSION: &str = "9.3.1";

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
        LibraryExtension::Map,
        LibraryExtension::Filter,
        LibraryExtension::Partial,
        LibraryExtension::Debug,
        LibraryExtension::SetType,
    ]);
    json_module::register(&mut builder);
    print::print_function(&mut builder);
    print::fail_function(&mut builder);
    hash::hash_function(&mut builder);
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
    /// Identifies the builtins, for naming the globals' heaps.
    pub id: String,
}

/// The values `native.legacy_globals` holds: those of the globals it's
/// named after, from globals built without it.
struct LegacyGlobals<'a> {
    values: Vec<(String, FrozenValue)>,
    heap: &'a FrozenHeapRef,
}

/// Add `native.bazel_version`, and `native.legacy_globals` once its values
/// are known.
fn native_values(native: &mut GlobalsBuilder, legacy: Option<&LegacyGlobals<'_>>) {
    native.set("bazel_version", BAZEL_VERSION);
    let Some(legacy) = legacy else {
        return;
    };
    native.frozen_heap().add_reference(legacy.heap);
    let fields: Vec<(&str, FrozenValue)> = legacy.values.iter().map(|(name, value)| (name.as_str(), *value)).collect();
    let legacy = native.alloc(starlark::values::structs::AllocStruct(fields));
    native.set("legacy_globals", legacy);
}

/// Globals that `build` makes, with `native.legacy_globals` holding the
/// values of the globals it names, as they are when built without it. Those
/// bb doesn't have are left out.
fn with_legacy_globals(build: impl Fn(Option<&LegacyGlobals<'_>>) -> Globals) -> Globals {
    let without = build(None);
    let values = without
        .iter()
        .filter(|(name, _)| opaque::LEGACY_GLOBALS.contains(name))
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    let legacy = LegacyGlobals {
        values,
        heap: without.heap(),
    };
    build(Some(&legacy))
}

/// The name of globals `with_legacy_globals` builds: `with` for those it
/// returns, and `without` for those it builds first. Both stay live, and
/// heaps are told apart by name when they're paged.
fn pass_name(legacy: Option<&LegacyGlobals<'_>>, with: &'static str, without: &'static str) -> &'static str {
    match legacy {
        Some(_) => with,
        None => without,
    }
}

fn build_globals_builder() -> GlobalsBuilder {
    let mut builder = standard()
        .with(build_api::build_functions)
        .with(select::select_function)
        .with(label_value::label_constructor)
        .with(depset::depset_function);
    native_rules::register(&mut builder);
    builder.frozen_heap().add_reference(provider::builtin_providers_heap());
    builder
}

/// Globals a higher layer adds to `.bzl` files, such as the real `cc_common`
/// analysis provides. They must be registered before any file is evaluated.
static EXTENSIONS: std::sync::Mutex<Vec<fn(&mut GlobalsBuilder)>> = std::sync::Mutex::new(Vec::new());

/// Add globals to every `.bzl` file, replacing builtins of the same names.
/// Call this before evaluating anything: files evaluated before don't see
/// them.
pub fn register_bzl_extension(extension: fn(&mut GlobalsBuilder)) {
    let mut extensions = EXTENSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !extensions.contains(&extension) {
        extensions.push(extension);
    }
}

/// Members a higher layer gives `_builtins.internal`, the native API
/// `@_builtins` is written against, such as `cc_internal`. They must be
/// registered before `@_builtins` is evaluated.
static BUILTINS_INTERNALS: std::sync::Mutex<Vec<fn(&mut GlobalsBuilder)>> = std::sync::Mutex::new(Vec::new());

/// Add members to `_builtins.internal`: each global `internal` sets becomes
/// one. Call this before evaluating anything.
pub fn register_builtins_internal(internal: fn(&mut GlobalsBuilder)) {
    let mut internals = BUILTINS_INTERNALS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !internals.contains(&internal) {
        internals.push(internal);
    }
}

/// `_builtins`, as `@_builtins` sees it: `internal` with the registered
/// members and `py_internal`, `toplevel` with `exec_transition`, and
/// everything else opaque.
fn builtins_object(builder: &mut GlobalsBuilder, names: (&'static str, &'static str)) -> FrozenValue {
    let internals = BUILTINS_INTERNALS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let mut members = GlobalsBuilder::new();
    transition::register_py_internal(&mut members);
    for internal in internals {
        internal(&mut members);
    }
    let internal = namespace(builder, "_builtins.internal", members, names.0);
    let toplevel = namespace(builder, "_builtins.toplevel", GlobalsBuilder::new().with(transition::exec_transition_function), names.1);
    let members = vec![("internal".to_string(), internal), ("toplevel".to_string(), toplevel)];
    builder.alloc(opaque::Namespace::new("_builtins", members))
}

/// A namespace of `_builtins` with `members`, which are kept in a heap of
/// their own named `heap`.
fn namespace(builder: &mut GlobalsBuilder, name: &str, members: GlobalsBuilder, heap: &'static str) -> FrozenValue {
    let members = members.build_named(GlobalFrozenHeapName { name: heap });
    builder.frozen_heap().add_reference(members.heap());
    let members = members.iter().map(|(name, value)| (name.to_string(), value)).collect();
    builder.alloc(opaque::Namespace::new(name, members))
}

fn bzl_globals_builder() -> GlobalsBuilder {
    let mut builder = standard()
        .with(select::select_function)
        .with(label_value::label_constructor)
        .with(rule_value::rule_function)
        .with(subrule::subrule_function)
        .with(transition::transition_function)
        .with(aspect::aspect_function)
        .with(provider::provider_function)
        .with(bzl_api::bzl_functions)
        .with(bzl_api::macro_function)
        .with(depset::depset_function)
        .with(repo_api::repo_functions);
    attr_module::register(&mut builder);
    toolchain_type::register(&mut builder);
    config_module::register(&mut builder);
    for name in opaque::BUILTIN_NAMESPACES {
        let value = builder.alloc(opaque::Opaque::new(name));
        builder.set(name, value);
    }
    provider::register_builtin_providers(&mut builder);
    builder.frozen_heap().add_reference(provider::builtin_providers_heap());
    builder
}

/// Add the registered extensions' globals, which replace any of the same
/// names.
fn add_extensions(builder: &mut GlobalsBuilder) {
    let extensions = EXTENSIONS.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    for extension in extensions {
        extension(builder);
    }
}

/// The globals of BUILD files, before builtins.
pub static BUILD_GLOBALS: LazyLock<Globals> = LazyLock::new(|| build_globals_builder().build_named(GlobalFrozenHeapName { name: "bb build" }));

/// The globals of `.bzl` files, before builtins. `@_builtins` itself is
/// evaluated with these and a stand-in `_builtins` object.
pub static BZL_GLOBALS: LazyLock<Globals> = LazyLock::new(|| {
    with_legacy_globals(|legacy| {
        let mut builder = bzl_globals_builder();
        builder.namespace("native", |native| {
            build_api::build_functions(native);
            native_rules::register(native);
            native_values(native, legacy);
        });
        let heaps = (
            pass_name(legacy, "bb builtins internal", "bb builtins internal without legacy_globals"),
            pass_name(legacy, "bb builtins toplevel", "bb builtins toplevel without legacy_globals"),
        );
        let value = builtins_object(&mut builder, heaps);
        builder.set("_builtins", value);
        add_extensions(&mut builder);
        builder.build_named(GlobalFrozenHeapName {
            name: pass_name(legacy, "bb bzl", "bb bzl without legacy_globals"),
        })
    })
});

/// The globals of BUILD files, with builtin rules.
pub fn build_globals(builtins: &StarlarkBuiltins<'_>) -> Globals {
    let name = globals_name("bb build", builtins);
    let mut builder = build_globals_builder();
    builder.frozen_heap().add_reference(builtins.heap);
    for (name, value) in &builtins.rules {
        builder.set(name, *value);
    }
    builder.build_named(name)
}

/// A name for globals made with `builtins`, unique to them. Globals are
/// made once per set of builtins, so the name is leaked.
fn globals_name(what: &str, builtins: &StarlarkBuiltins<'_>) -> GlobalFrozenHeapName {
    let name: &'static str = Box::leak(format!("{what} {}", builtins.id).into_boxed_str());
    GlobalFrozenHeapName { name }
}

/// The globals of `.bzl` files, with builtin rules and values.
pub fn bzl_globals(builtins: &StarlarkBuiltins<'_>) -> Globals {
    with_legacy_globals(|legacy| {
        let name = globals_name(pass_name(legacy, "bb bzl", "bb bzl without legacy_globals"), builtins);
        let mut builder = bzl_globals_builder();
        builder.frozen_heap().add_reference(builtins.heap);
        for (name, value) in &builtins.toplevels {
            builder.set(name, *value);
        }
        add_extensions(&mut builder);
        builder.namespace("native", |native| {
            build_api::build_functions(native);
            native_rules::register(native);
            native_values(native, legacy);
            native.frozen_heap().add_reference(builtins.heap);
            for (name, value) in &builtins.rules {
                native.set(name, *value);
            }
        });
        builder.build_named(name)
    })
}
