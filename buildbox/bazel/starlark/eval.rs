//! Evaluating BUILD and `.bzl` files.

use crate::context::{EvalContext, FileKind};
use crate::package_builder::PackageBuilder;
use crate::dialect;
use starlark::environment::Globals;
use bazel_label::Label;
use bazel_packages::Package;
use starlark::environment::{FrozenModule, Module};
use starlark::values::OwnedFrozenValue;
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
}

impl EvalError {
    fn new(error: impl std::fmt::Display) -> EvalError {
        EvalError {
            message: error.to_string(),
        }
    }
}

/// Parse a file, named by its label so that builtins can tell which file
/// called them.
pub fn parse(file: Label, source: String) -> Result<AstModule, EvalError> {
    AstModule::parse(&file.canonical_form_for_file(), source, &dialect()).map_err(EvalError::new)
}

/// The modules a file's `load` statements name, as written.
pub fn loads(ast: &AstModule) -> Vec<String> {
    ast.loads().into_iter().map(|load| load.module_id.to_string()).collect()
}

/// The names a file's `load` statements bind.
pub fn loaded_names(ast: &AstModule) -> Vec<String> {
    ast.loads()
        .into_iter()
        .flat_map(|load| load.symbols.keys().map(|name| name.to_string()).collect::<Vec<_>>())
        .collect()
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

/// Evaluate a `.bzl` file, returning its frozen module and what it printed.
pub fn eval_bzl(ast: AstModule, globals: &Globals, loads: &Loads<'_>, context: EvalContext) -> Result<(FrozenModule, Vec<String>), EvalError> {
    debug_assert_eq!(context.kind, FileKind::Bzl);
    Module::with_temp_heap(|module| {
        {
            let printer = Printer(&context);
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&context);
            eval.set_loader(loads);
            eval.set_print_handler(&printer);
            eval.eval_module(ast, globals).map_err(EvalError::new)?;
        }
        let frozen = module.freeze().map_err(|error| EvalError::new(format!("{error:?}")))?;
        Ok((frozen, context.printed.take()))
    })
}

/// Evaluate a BUILD file into a package, and what it printed. An error
/// partway through still returns the targets defined before it, with the
/// error recorded in the package. `predeclared` are globals the file sees
/// without loading them, such as autoloaded rules.
pub fn eval_build(
    ast: AstModule,
    globals: &Globals,
    loads: &Loads<'_>,
    predeclared: &[(String, OwnedFrozenValue)],
    mut context: EvalContext,
    builder: PackageBuilder,
) -> (Package, Vec<String>) {
    debug_assert_eq!(context.kind, FileKind::Build);
    context.package = Some(std::cell::RefCell::new(builder));
    let result = Module::with_temp_heap(|module| {
        for (name, value) in predeclared {
            module.set(name, module.heap().access_owned_frozen_value(value));
        }
        let printer = Printer(&context);
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&context);
        eval.set_loader(loads);
        eval.set_print_handler(&printer);
        eval.eval_module(ast, globals).map(|_| ()).map_err(EvalError::new)
    });
    let printed = context.printed.take();
    let mut builder = context.package.take().expect("set above").into_inner();
    if let Err(error) = result {
        builder.errors.push(error.message);
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
