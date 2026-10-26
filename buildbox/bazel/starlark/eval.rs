//! Evaluating BUILD and `.bzl` files.

use crate::context::{EvalContext, FileKind};
use crate::package_builder::PackageBuilder;
use crate::dialect;
use starlark::values::FrozenHeapName;
use starlark::environment::Globals;
use bazel_label::Label;
use bazel_packages::{Package, PackageError, SourcePosition};
use crate::reported;
use starlark::environment::{FrozenModule, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::AstModule;
use crate::print::Printed;
use std::collections::HashMap;

/// Why a file couldn't be parsed or evaluated, with the Starlark error's
/// location and backtrace.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EvalError {
    pub message: String,
    /// The error in a line, as Bazel words it: `line:column: message`, or
    /// just the message if it has no location.
    pub brief: String,
    /// The error as Bazel reports it.
    pub reported: PackageError,
}

impl EvalError {
    fn new(error: impl std::fmt::Display) -> EvalError {
        let message = error.to_string();
        EvalError {
            brief: message.clone(),
            reported: PackageError::new(message.clone()),
            message,
        }
    }

    fn starlark(error: starlark::Error) -> EvalError {
        EvalError::with_candidates(error, &[])
    }

    /// The error, with `candidates` the names in scope to suggest for an
    /// undefined one.
    fn with_candidates(error: starlark::Error, candidates: &[String]) -> EvalError {
        let described = reported::bazel_wording(&error.without_diagnostic().to_string(), candidates);
        let brief = match error.span() {
            Some(span) => {
                let start = span.resolve_span().begin;
                format!("{}:{}: {described}", start.line + 1, start.column + 1)
            }
            None => described,
        };
        EvalError {
            reported: reported::package_error(&error, candidates),
            message: error.to_string(),
            brief,
        }
    }
}

/// The names a file sees: `globals`', and those its `load`s bind.
fn names_in_scope(ast: &AstModule, globals: &Globals) -> Vec<String> {
    let mut names: Vec<String> = globals.iter().map(|(name, _)| name.to_string()).collect();
    for load in ast.loads() {
        names.extend(load.symbols.keys().map(|name| name.to_string()));
    }
    names
}

/// Parse a file, named by its label so that builtins can tell which file
/// called them.
pub fn parse(file: Label, source: String) -> Result<AstModule, EvalError> {
    AstModule::parse(&file.canonical_form_for_file(), source, &dialect()).map_err(EvalError::starlark)
}

/// The most syntax errors a file reports, as Bazel limits them, so that a
/// file that's badly broken doesn't flood the output.
const MAX_SYNTAX_ERRORS: usize = 5;

/// Parse a file, as [`parse`], failing with each of its syntax errors, up
/// to [`MAX_SYNTAX_ERRORS`], as Bazel reports them. A file with syntax
/// errors has its names checked only once they're fixed.
pub fn parse_reporting_all(file: Label, source: String) -> Result<AstModule, Vec<PackageError>> {
    AstModule::parse_all_errors(&file.canonical_form_for_file(), source, &dialect())
        .map_err(|errors| errors.into_iter().take(MAX_SYNTAX_ERRORS).map(|error| EvalError::starlark(error).reported).collect())
}

/// The modules a file's `load` statements name, as written.
pub fn loads(ast: &AstModule) -> Vec<String> {
    ast.loads().into_iter().map(|load| load.module_id.to_string()).collect()
}

/// Where each module a file's `load` statements name is named: where its
/// string starts, as Bazel places errors loading it.
pub fn load_positions(ast: &AstModule) -> Vec<SourcePosition> {
    let mut positions = Vec::new();
    for load in ast.loads() {
        let mut position = reported::start_position(&load.span);
        let source = load.span.source_span();
        if let Some(offset) = source.find(['"', '\'']) {
            position.column += source[..offset].chars().count() as u32;
        }
        positions.push(position);
    }
    positions
}

/// Gives evaluation the modules a file loads, by the strings its `load`
/// statements use.
pub struct Loads<'a> {
    pub modules: HashMap<String, &'a FrozenModule>,
}

impl FileLoader for Loads<'_> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        match self.modules.get(path) {
            Some(module) => Ok((*module).clone()),
            None => Err(starlark::Error::new_other(anyhow::anyhow!("cannot load '{path}': not resolved"))),
        }
    }
}

/// The name of a frozen heap: what it holds, and a fingerprint of what it
/// was made from. Heaps are told apart by name when they're serialized, so
/// heaps made from different things must have different names.
pub fn heap_name(what: &str, label: impl std::fmt::Display, fingerprint: impl std::fmt::Display) -> FrozenHeapName {
    FrozenHeapName::User(Box::new(format!("{what} {label} {fingerprint}")))
}

/// Evaluate a `.bzl` file, returning its frozen module, its heap named
/// `name`, and what it printed, even if it failed.
pub fn eval_bzl(ast: AstModule, globals: &Globals, loads: &Loads<'_>, context: EvalContext, name: FrozenHeapName) -> (Result<FrozenModule, EvalError>, Vec<Printed>) {
    debug_assert_eq!(context.kind, FileKind::Bzl);
    let module = Module::with_temp_heap(|module| {
        module.frozen_heap().add_reference(crate::provider::builtin_providers_heap());
        {
            let candidates = names_in_scope(&ast, globals);
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.set_loader(loads);
            eval.eval_module(ast, globals).map_err(|error| EvalError::with_candidates(error, &candidates))?;
        }
        module.freeze_named(name).map_err(|error| EvalError::new(format!("{error:?}")))
    });
    (module, context.printed.take())
}

/// Evaluate a BUILD file into a package, and what it printed. An error
/// partway through still returns the targets defined before it, with the
/// error recorded in the package.
pub fn eval_build(
    ast: AstModule,
    globals: &Globals,
    loads: &Loads<'_>,
    mut context: EvalContext,
    builder: PackageBuilder,
) -> (Package, Vec<Printed>) {
    debug_assert_eq!(context.kind, FileKind::Build);
    context.package = Some(std::cell::RefCell::new(builder));
    let candidates = names_in_scope(&ast, globals);
    let result = Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&context);
        eval.set_loader(loads);
        eval.eval_module_all_errors(ast, globals).map(|_| ())
    });
    let printed = context.printed.take();
    let mut builder = context.package.take().expect("set above").into_inner();
    if let Err(errors) = result {
        for error in errors {
            builder.errors.push(EvalError::with_candidates(error, &candidates).reported);
        }
    }
    (builder.finish(), printed)
}

/// The file name a file is parsed under: its canonical label.
trait FileName {
    fn canonical_form_for_file(&self) -> String;
}

impl FileName for Label {
    fn canonical_form_for_file(&self) -> String {
        if self.repo().is_main() {
            return self.to_string();
        }
        format!("@@{}//{}:{}", self.repo(), self.package.path, self.name)
    }
}
