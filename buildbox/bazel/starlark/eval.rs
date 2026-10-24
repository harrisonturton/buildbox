//! Evaluating BUILD and `.bzl` files.

use crate::context::{EvalContext, FileKind};
use crate::package_builder::PackageBuilder;
use crate::dialect;
use starlark::values::FrozenHeapName;
use starlark::environment::Globals;
use bazel_label::Label;
use bazel_packages::Package;
use starlark::environment::{FrozenModule, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::AstModule;
use starlark::PrintHandler;
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
}

impl EvalError {
    fn new(error: impl std::fmt::Display) -> EvalError {
        let message = error.to_string();
        EvalError {
            brief: message.clone(),
            message,
        }
    }

    fn starlark(error: starlark::Error) -> EvalError {
        let described = bazel_wording(&error.without_diagnostic().to_string());
        let brief = match error.span() {
            Some(span) => {
                let start = span.resolve_span().begin;
                format!("{}:{}: {described}", start.line + 1, start.column + 1)
            }
            None => described,
        };
        EvalError {
            message: error.to_string(),
            brief,
        }
    }
}

/// A Starlark error's message as Bazel's Starlark words it, for the errors
/// whose wording differs.
fn bazel_wording(message: &str) -> String {
    let message = message.trim();
    if let Some(name) = message.strip_prefix("Variable `").and_then(|rest| rest.strip_suffix("` not found")) {
        return format!("name '{name}' is not defined");
    }
    if let Some(rest) = message.strip_prefix("Parse error: unexpected ") {
        return syntax_error(rest).unwrap_or_else(|| format!("syntax error: {rest}"));
    }
    message.strip_prefix("error: ").unwrap_or(message).to_string()
}

/// `syntax error at 'cmd': expected )`, from starlark-rust's
/// `identifier 'cmd', expected symbol ')'`.
fn syntax_error(unexpected: &str) -> Option<String> {
    let (found, expected) = unexpected.split_once(", expected ")?;
    let token = found.split_once(' ').map(|(_, token)| token).unwrap_or(found).trim_matches('\'');
    let expected = expected.strip_prefix("symbol ").unwrap_or(expected).trim_matches('\'');
    Some(format!("syntax error at '{token}': expected {expected}"))
}

/// Parse a file, named by its label so that builtins can tell which file
/// called them.
pub fn parse(file: Label, source: String) -> Result<AstModule, EvalError> {
    AstModule::parse(&file.canonical_form_for_file(), source, &dialect()).map_err(EvalError::starlark)
}

/// The modules a file's `load` statements name, as written.
pub fn loads(ast: &AstModule) -> Vec<String> {
    ast.loads().into_iter().map(|load| load.module_id.to_string()).collect()
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

struct Printer<'a>(&'a EvalContext);

impl PrintHandler for Printer<'_> {
    fn println(&self, text: &str) -> starlark::Result<()> {
        self.0.printed.borrow_mut().push(text.to_string());
        Ok(())
    }
}

/// The name of a frozen heap: what it holds, and a fingerprint of what it
/// was made from. Heaps are told apart by name when they're serialized, so
/// heaps made from different things must have different names.
pub fn heap_name(what: &str, label: impl std::fmt::Display, fingerprint: impl std::fmt::Display) -> FrozenHeapName {
    FrozenHeapName::User(Box::new(format!("{what} {label} {fingerprint}")))
}

/// Evaluate a `.bzl` file, returning its frozen module, its heap named
/// `name`, and what it printed.
pub fn eval_bzl(ast: AstModule, globals: &Globals, loads: &Loads<'_>, context: EvalContext, name: FrozenHeapName) -> Result<(FrozenModule, Vec<String>), EvalError> {
    debug_assert_eq!(context.kind, FileKind::Bzl);
    Module::with_temp_heap(|module| {
        module.frozen_heap().add_reference(crate::provider::builtin_providers_heap());
        {
            let printer = Printer(&context);
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.set_loader(loads);
            eval.set_print_handler(&printer);
            eval.eval_module(ast, globals).map_err(EvalError::new)?;
        }
        let frozen = module.freeze_named(name).map_err(|error| EvalError::new(format!("{error:?}")))?;
        Ok((frozen, context.printed.take()))
    })
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
) -> (Package, Vec<String>) {
    debug_assert_eq!(context.kind, FileKind::Build);
    context.package = Some(std::cell::RefCell::new(builder));
    let result = Module::with_temp_heap(|module| {
        let printer = Printer(&context);
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&context);
        eval.set_loader(loads);
        eval.set_print_handler(&printer);
        eval.eval_module(ast, globals).map(|_| ()).map_err(EvalError::starlark)
    });
    let printed = context.printed.take();
    let mut builder = context.package.take().expect("set above").into_inner();
    if let Err(error) = result {
        builder.errors.push(error.brief);
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
