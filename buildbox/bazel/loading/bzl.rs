//! Loading `.bzl` files.

use crate::package::{PackageLookup, PackageLookupKey};
use crate::{RepoMappingKey, RepoRootKey, blocking};
use bazel_fs::RootedPath;
use bazel_label::{Label, LabelContext, RepoMappingTable, RepoName};
use bazel_packages::{PackageError, SourcePosition};
use bazel_starlark::{EvalContext, FileKind, Loads};
use skycodec::{Decode, Encode, Fingerprint, Fingerprinter};
use skyframe::{Ctx, Key, SkyError, Storage, Value};
use starlark::environment::FrozenModule;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// An evaluated `.bzl` file.
///
/// Modules are live Starlark objects, so they're held as they are rather
/// than encoded. Workspaces that load the same file with the same contents
/// and loads share one module.
pub struct BzlModule {
    pub label: Label,
    pub module: FrozenModule,
    /// Identifies the module's source, the repository mapping it was
    /// evaluated with, and every module it loads.
    pub fingerprint: Fingerprint,
    /// Every repository whose files this module, or a module it loads, came
    /// from, which builtins need the mappings of.
    pub repos: Vec<RepoName>,
    /// The modules this one loads directly.
    pub loads: Vec<Label>,
}

impl Value for BzlModule {
    fn to_storage(&self) -> Storage {
        Storage::Live {
            fingerprint: self.fingerprint,
            size: self.module.frozen_heap().allocated_bytes(),
        }
    }

    fn from_bytes(_bytes: &[u8]) -> Result<BzlModule, skycodec::DecodeError> {
        Err(skycodec::DecodeError::Invalid {
            what: "live module",
            value: 0,
            position: 0,
        })
    }
}

/// Loads the `.bzl` file a label names.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BzlLoadKey(pub Label);

impl Key for BzlLoadKey {
    type Value = BzlModule;
    const KIND: &'static str = "BZL_LOAD";

    async fn compute(&self, ctx: &Ctx) -> Result<BzlModule, SkyError> {
        let label = self.0;
        let source = read_bzl(ctx, label).await?;
        let mapping = ctx.compute(&RepoMappingKey(label.repo())).await?;
        let ast = match bazel_starlark::parse_reporting_all(label, source.clone()) {
            Ok(ast) => ast,
            Err(errors) => return Err(failed_module(ctx, label, &errors, "compilation").await),
        };
        let positions = bazel_starlark::load_positions(&ast);
        let loaded = load_all(ctx, label, &bazel_starlark::loads(&ast), Some(&positions), &mapping.0).await?;
        let repos = transitive_repos(label.repo(), loaded.iter().map(|(_, module)| &**module));
        let mappings = mappings(ctx, &repos).await?;
        let builtins = ctx.compute(&crate::BuiltinsKey).await?;
        let fingerprint = fingerprint(label, &source, &mapping.0, &loaded);
        let load_labels = loaded.iter().map(|(_, module)| module.label).collect();

        let module = blocking(move || {
            let loads = Loads {
                modules: loaded.iter().map(|(load, module)| (load.clone(), &module.module)).collect(),
            };
            let mut context = EvalContext::new(FileKind::Bzl, label);
            context.mappings = mappings;
            bazel_starlark::eval_bzl(ast, &builtins.bzl, &loads, context, bazel_starlark::heap_name("bzl", label, fingerprint))
        })
        .await?;
        let (module, printed) = module;
        crate::report_printed(ctx, &printed).await;
        let module = match module {
            Ok(module) => module,
            Err(error) => {
                let phase = if error.reported.position.is_some() { "compilation" } else { "initialization" };
                return Err(failed_module(ctx, label, &[error.reported], phase).await);
            }
        };
        Ok(BzlModule {
            label,
            module,
            fingerprint,
            repos,
            loads: load_labels,
        })
    }
}

/// The error of a module that failed to compile or initialize, `phase`, as
/// Bazel reports it: the errors it failed with, as they were reported, then
/// that it failed, which a module or package loading it gives as why.
async fn failed_module(ctx: &Ctx, label: Label, errors: &[PackageError], phase: &str) -> SkyError {
    let mut reported = Vec::with_capacity(errors.len() + 1);
    for error in errors {
        let paths = crate::error_paths(ctx, error).await;
        reported.push(error.report(&|file| paths.get(file).cloned().unwrap_or_else(|| file.to_string())));
    }
    let path = label.package.path.join_path(intern::PathId::parse(label.name.as_str()).unwrap_or(intern::PathId::ROOT));
    reported.push(format!("{phase} of module '{path}' failed"));
    SkyError::new(reported.join(REPORTED_SEPARATOR))
}

/// What separates the errors an error's message holds, which are reported
/// each as its own error: those reported as they happened, then the last,
/// which they led to.
const REPORTED_SEPARATOR: &str = "\nERROR: ";

/// A message's errors reported as they happened, and its last, which they
/// led to.
pub fn split_reported(message: &str) -> (&str, &str) {
    message.rsplit_once(REPORTED_SEPARATOR).unwrap_or(("", message))
}

/// An error with its last error, which the others led to, reworded by
/// `reword`, as each layer that the error fails gives its own reason.
pub fn reworded(error: &SkyError, reword: impl FnOnce(&str) -> String) -> SkyError {
    let (reported, last) = split_reported(error.message());
    let last = reword(last);
    let message = match reported.is_empty() {
        true => last,
        false => format!("{reported}{REPORTED_SEPARATOR}{last}"),
    };
    let reworded = SkyError::with_kind(error.kind(), message);
    match error.origin() {
        Some(origin) => reworded.with_origin(origin),
        None => reworded,
    }
}

/// Read a `.bzl` file, which must be in a package.
async fn read_bzl(ctx: &Ctx, label: Label) -> Result<String, SkyError> {
    let lookup = ctx.compute(&PackageLookupKey(label.package)).await?;
    if let PackageLookup::NotFound { reason } = &*lookup {
        return Err(SkyError::new(format!(
            "cannot load '{label}': {reason}"
        )));
    }
    let root = ctx.compute(&RepoRootKey(label.repo())).await?;
    let path = label.package.path.join_path(intern::PathId::parse(label.name.as_str()).map_err(|error| SkyError::new(error.to_string()))?);
    let Some(bytes) = bazel_fs::read_file(ctx, RootedPath::new(root.0, path)).await? else {
        return Err(SkyError::new(format!("cannot load '{label}': no such file")));
    };
    String::from_utf8(bytes).map_err(|_| SkyError::new(format!("cannot load '{label}': not valid UTF-8")))
}

/// Resolve and load the modules a file's `load` statements name, in
/// parallel. For a `.bzl` file, `positions` are where its statements name
/// them, at which a module that failed to load is reported, as Bazel
/// reports it.
pub(crate) async fn load_all(
    ctx: &Ctx,
    from: Label,
    loads: &[String],
    positions: Option<&[SourcePosition]>,
    mapping: &RepoMappingTable,
) -> Result<Vec<(String, Arc<BzlModule>)>, SkyError> {
    let context = LabelContext {
        package: from.package,
        mapping,
        lenient: false,
    };
    let mut keys = Vec::with_capacity(loads.len());
    for load in loads {
        let label = Label::parse(load, &context).map_err(|error| SkyError::new(format!("{from}: in load statement: {error}")))?;
        if !label.name.as_str().ends_with(".bzl") && !label.name.as_str().ends_with(".scl") {
            return Err(SkyError::new(format!("{from}: the label '{load}' must refer to a .bzl file")));
        }
        keys.push(BzlLoadKey(label));
    }
    let modules = ctx.compute_many(&keys).await;
    let mut loaded = Vec::with_capacity(loads.len());
    for (index, (load, module)) in loads.iter().zip(modules).enumerate() {
        let module = match (module, positions.and_then(|positions| positions.get(index))) {
            (Ok(module), _) => module,
            (Err(error), Some(position)) => return Err(failed_load(ctx, from, position, &error).await),
            (Err(error), None) => return Err(error),
        };
        loaded.push((load.clone(), module));
    }
    Ok(loaded)
}

/// The error of a `.bzl` file whose load of a module failed with `error`,
/// as Bazel words it: at the load.
async fn failed_load(ctx: &Ctx, from: Label, position: &SourcePosition, error: &SkyError) -> SkyError {
    let path = match crate::source_path(ctx, from).await {
        Some(path) => path.display().to_string(),
        None => from.to_string(),
    };
    reworded(error, |why| format!("at {path}:{}:{}: {why}", position.line, position.column))
}

/// The repositories whose mappings code in a file may need: its own, those
/// of the modules it loads, and `@_builtins`, whose functions any file can
/// call.
pub(crate) fn transitive_repos<'a>(own: RepoName, loaded: impl Iterator<Item = &'a BzlModule>) -> Vec<RepoName> {
    let mut repos = BTreeSet::from([own, RepoName::new(crate::BUILTINS_REPO)]);
    for module in loaded {
        repos.extend(module.repos.iter().copied());
    }
    repos.into_iter().collect()
}

/// The mappings of every repository in `repos`.
pub async fn mappings(ctx: &Ctx, repos: &[RepoName]) -> Result<HashMap<RepoName, Arc<RepoMappingTable>>, SkyError> {
    let keys: Vec<RepoMappingKey> = repos.iter().map(|repo| RepoMappingKey(*repo)).collect();
    let mut mappings = HashMap::with_capacity(repos.len());
    for (repo, mapping) in repos.iter().zip(ctx.compute_many(&keys).await) {
        mappings.insert(*repo, Arc::new(mapping?.0.clone()));
    }
    Ok(mappings)
}

pub(crate) fn fingerprint(label: Label, source: &str, mapping: &RepoMappingTable, loaded: &[(String, Arc<BzlModule>)]) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(label.canonical_form().as_bytes());
    fingerprinter.bytes(source.as_bytes());
    fingerprinter.bytes(&skycodec::to_bytes(mapping));
    for (load, module) in loaded {
        fingerprinter.bytes(load.as_bytes());
        fingerprinter.fingerprint(&module.fingerprint);
    }
    fingerprinter.finish()
}
