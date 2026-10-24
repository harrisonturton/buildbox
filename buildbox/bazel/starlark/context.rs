//! What builtins know about the evaluation they're called from.

use crate::convert::LabelParser;
use crate::package_builder::PackageBuilder;
use bazel_label::{Label, LabelContext, PackageId, RepoMapping, RepoMappingTable, RepoName};
use starlark::any::ProvidesStaticType;
use starlark::eval::Evaluator;
use std::cell::{RefCell, RefMut};
use std::collections::HashMap;
use std::sync::Arc;

/// The kind of file being evaluated, which decides the builtins available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Bzl,
    Build,
}

/// Lists files for `glob()`.
pub trait Globber {
    fn glob(&self, include: &[String], exclude: &[String], exclude_directories: bool, allow_empty: bool) -> anyhow::Result<Vec<String>>;
}

/// The context of one evaluation, which builtins reach through
/// `Evaluator::extra`.
///
/// Every file is evaluated with its canonical label as its file name, so a
/// builtin can tell which file called it, and resolve labels relative to
/// that file's package and repository.
#[derive(ProvidesStaticType)]
pub struct EvalContext {
    pub kind: FileKind,
    /// The file being evaluated.
    pub file: Label,
    /// How each repository involved sees the others.
    pub mappings: HashMap<RepoName, Arc<RepoMappingTable>>,
    /// The package a BUILD file is defining.
    pub package: Option<RefCell<PackageBuilder>>,
    pub globber: Option<Box<dyn Globber>>,
    /// The name and version of the module the file's repository belongs to.
    pub module: Option<(String, String)>,
    /// The repositories a module extension defines.
    pub repos: Option<RefCell<Vec<crate::repo_api::RepoDefinition>>>,
    /// What `print()` printed.
    pub printed: RefCell<Vec<String>>,
    /// The state of a higher layer's evaluation, such as the rule being
    /// analyzed, for the builtins that layer adds.
    pub extension: Option<Box<dyn std::any::Any>>,
}

impl EvalContext {
    pub fn new(kind: FileKind, file: Label) -> EvalContext {
        EvalContext {
            kind,
            file,
            mappings: HashMap::new(),
            package: None,
            globber: None,
            repos: None,
            module: None,
            printed: RefCell::new(Vec::new()),
            extension: None,
        }
    }

    /// The context of the evaluation `eval` is running.
    pub fn get<'a>(eval: &Evaluator<'_, 'a, '_>) -> anyhow::Result<&'a EvalContext> {
        let extra = eval.extra.ok_or_else(|| anyhow::anyhow!("no evaluation context"))?;
        extra
            .downcast_ref::<EvalContext>()
            .ok_or_else(|| anyhow::anyhow!("no evaluation context"))
    }

    /// How `repo` sees other repositories. A repository with no known mapping
    /// sees only itself.
    pub fn mapping_for(&self, repo: RepoName) -> Arc<dyn RepoMapping> {
        match self.mappings.get(&repo) {
            Some(mapping) => mapping.clone(),
            None => Arc::new(RepoMappingTable::new(repo, Vec::new())),
        }
    }

    /// The package being defined, failing outside BUILD files.
    pub fn package_builder(&self, what: &str) -> anyhow::Result<RefMut<'_, PackageBuilder>> {
        match &self.package {
            Some(package) => Ok(package.borrow_mut()),
            None => anyhow::bail!("{what} can only be used while evaluating a BUILD file"),
        }
    }

    /// The package of the BUILD file being evaluated.
    pub fn build_package(&self, what: &str) -> anyhow::Result<PackageId> {
        Ok(self.package_builder(what)?.id)
    }

    /// Parse a label relative to the BUILD file's package, as attribute
    /// values are.
    pub fn parse_in_package(&self, text: &str) -> anyhow::Result<Label> {
        let package = self.build_package("labels relative to a package")?;
        let mapping = self.mapping_for(package.repo);
        LabelParser {
            package,
            mapping: &*mapping,
        }
        .parse(text)
    }

    /// The file whose code made the current call: the innermost Starlark
    /// frame's file, or the file being evaluated.
    pub fn caller_file(&self, eval: &Evaluator<'_, '_, '_>) -> Label {
        let Some(location) = eval.call_stack_top_location() else {
            return self.file;
        };
        Label::parse_canonical(location.filename()).unwrap_or(self.file)
    }

    /// Parse a label relative to the file whose code made the current call,
    /// as `Label()` does.
    pub fn parse_label_in_caller(&self, text: &str, eval: &Evaluator<'_, '_, '_>) -> anyhow::Result<Label> {
        let caller = self.caller_file(eval);
        let mapping = self.mapping_for(caller.repo());
        let context = LabelContext {
            package: caller.package,
            mapping: &*mapping,
            lenient: false,
        };
        Ok(Label::parse(text, &context)?)
    }
}
