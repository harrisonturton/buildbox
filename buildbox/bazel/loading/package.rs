//! Loading packages from BUILD files.

use crate::bzl::{BzlModule, load_all, mappings, transitive_repos};
use crate::{RepoMappingKey, RepoRootKey, blocking};
use bazel_fs::{FileState, FileStateKey, Glob, GlobPattern, Root, RootedPath};
use bazel_label::{Label, PackageId};
use bazel_packages::Package;
use bazel_progress::Progress;
use bazel_starlark::{EvalContext, FileKind, Globber, Loads, PackageBuilder};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Compute, Ctx, Key, SkyError};
use bazel_label::RepoMappingTable;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The names a BUILD file may have, in order of preference.
pub const BUILD_FILE_NAMES: [&str; 2] = ["BUILD.bazel", "BUILD"];

/// Whether a package exists, and its BUILD file if so.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum PackageLookup {
    Found { root: Root, build_file: Symbol },
    NotFound { reason: String },
}

encoded_value!(PackageLookup);

/// Finds a package's BUILD file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PackageLookupKey(pub PackageId);

impl Key for PackageLookupKey {
    type Value = PackageLookup;
    const KIND: &'static str = "PACKAGE_LOOKUP";

    async fn compute(&self, ctx: &Ctx) -> Result<PackageLookup, SkyError> {
        let package = self.0;
        if let Some(reason) = package.repo.unknown_reason() {
            let reason = format!("The repository '@@{}' could not be resolved: {reason}", package.repo);
            return Ok(PackageLookup::NotFound { reason });
        }
        let root = ctx.compute(&RepoRootKey(package.repo)).await?.0;
        let keys: Vec<FileStateKey> = BUILD_FILE_NAMES
            .iter()
            .map(|name| FileStateKey(RootedPath::new(root, package.path.join(Symbol::intern(name)))))
            .collect();
        let states = ctx.compute_many(&keys).await;
        for (name, state) in BUILD_FILE_NAMES.iter().zip(states) {
            if let FileState::File { .. } = *state? {
                return Ok(PackageLookup::Found {
                    root,
                    build_file: Symbol::intern(name),
                });
            }
        }
        Ok(PackageLookup::NotFound {
            reason: format!("BUILD file not found in any of the following directories. Add a BUILD file to a directory to mark it as a package.\n - {}", describe_dir(package)),
        })
    }
}

fn describe_dir(package: PackageId) -> String {
    if package.repo.is_main() {
        return package.path.to_string();
    }
    format!("external/{}/{}", package.repo, package.path)
}

/// A loaded package, as a value of the graph.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct LoadedPackage(pub Package);

encoded_value!(LoadedPackage);

/// Loads a package by evaluating its BUILD file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PackageKey(pub PackageId);

impl Key for PackageKey {
    type Value = LoadedPackage;
    const KIND: &'static str = "PACKAGE";

    async fn compute(&self, ctx: &Ctx) -> Result<LoadedPackage, SkyError> {
        let loaded = load_package(ctx, self.0).await;
        if let Ok(progress) = ctx.global::<Progress>() {
            progress.package_loaded();
        }
        loaded
    }
}

/// Load the package `id`: evaluate its BUILD file.
async fn load_package(ctx: &Ctx, id: PackageId) -> Result<LoadedPackage, SkyError> {
    let (ast, prepared) = match prepare(ctx, id).await? {
        Prepared::Ready(ast, prepared) => (ast, prepared),
        Prepared::Failed(package) => return Ok(LoadedPackage(package)),
    };
    let load_labels = prepared.load_labels();
    let (mut package, printed) = blocking(move || {
        let (loads, context, builder) = prepared.evaluation();
        bazel_starlark::eval_build(ast, &prepared.globals, &loads, context, builder)
    })
    .await?;
    crate::report_printed(ctx, &printed).await;
    package.loads = load_labels;
    Ok(LoadedPackage(package))
}

/// A BUILD file's module, evaluated again and kept, for the rule classes
/// it defines with `testing.analysis_test`, whose implementations its
/// targets' analysis calls.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct BuildModuleKey(pub PackageId);

impl Key for BuildModuleKey {
    type Value = BzlModule;
    const KIND: &'static str = "BUILD_MODULE";

    async fn compute(&self, ctx: &Ctx) -> Result<BzlModule, SkyError> {
        let id = self.0;
        let (ast, prepared) = match prepare(ctx, id).await? {
            Prepared::Ready(ast, prepared) => (ast, prepared),
            Prepared::Failed(_) => return Err(SkyError::new(format!("no module for package '{}': its BUILD file failed", display(id)))),
        };
        let load_labels = prepared.load_labels();
        let label = prepared.build_label;
        let fingerprint = crate::bzl::fingerprint(label, &prepared.source, &prepared.mapping, &prepared.loaded, prepared.context);
        let repos = prepared.repos.clone();
        let name = bazel_starlark::heap_name("build", label, fingerprint);
        let (_, module) = blocking(move || {
            let (loads, context, builder) = prepared.evaluation();
            bazel_starlark::eval_build_module(ast, &prepared.globals, &loads, context, builder, name)
        })
        .await?;
        let module = module.map_err(|error| SkyError::new(format!("no module for package '{}': {error}", display(id))))?;
        Ok(BzlModule {
            label,
            module,
            fingerprint,
            repos,
            loads: load_labels,
        })
    }
}

/// What evaluating a BUILD file needs, or the package it is if it can't be
/// parsed.
enum Prepared {
    Ready(starlark::syntax::AstModule, Box<Evaluation>),
    Failed(Package),
}

/// A BUILD file, with the modules it loads.
struct Evaluation {
    id: PackageId,
    build_file: Symbol,
    build_label: Label,
    source: String,
    mapping: Arc<RepoMappingTable>,
    loaded: Vec<(String, Arc<BzlModule>)>,
    repos: Vec<bazel_label::RepoName>,
    mappings: std::collections::HashMap<bazel_label::RepoName, Arc<RepoMappingTable>>,
    /// The globals the BUILD file is evaluated with.
    globals: starlark::environment::Globals,
    /// The context its `.bzl` files are loaded in, which tells its module
    /// apart from others of the same file.
    context: crate::bzl::BzlContext,
    module: Option<(String, String)>,
    globber: GraphGlobber,
}

impl Evaluation {
    fn load_labels(&self) -> Vec<Label> {
        self.loaded.iter().map(|(_, module)| module.label).collect()
    }

    /// What evaluating the BUILD file takes besides its syntax and globals.
    fn evaluation(&self) -> (Loads<'_>, EvalContext, PackageBuilder) {
        let loads = Loads {
            modules: self.loaded.iter().map(|(load, module)| (load.clone(), &module.module)).collect(),
        };
        let mut context = EvalContext::new(FileKind::Build, self.build_label);
        context.mappings = self.mappings.clone();
        context.globber = Some(Box::new(self.globber.clone()));
        context.module = self.module.clone();
        (loads, context, PackageBuilder::new(self.id, self.build_file))
    }
}

/// Read and parse the BUILD file of `id`, and load what it loads.
async fn prepare(ctx: &Ctx, id: PackageId) -> Result<Prepared, SkyError> {
    let lookup = ctx.compute(&PackageLookupKey(id)).await?;
    let (root, build_file) = match &*lookup {
        PackageLookup::Found { root, build_file } => (*root, *build_file),
        PackageLookup::NotFound { reason } => return Err(SkyError::new(format!("no such package '{}': {reason}", display(id)))),
    };
    let build_label = id.label(build_file);
    let Some(bytes) = bazel_fs::read_file(ctx, RootedPath::new(root, id.path.join(build_file))).await? else {
        return Err(SkyError::transient(format!("no such package '{}': BUILD file disappeared", display(id))));
    };
    let source = String::from_utf8(bytes).map_err(|_| SkyError::new(format!("{build_label}: not valid UTF-8")))?;

    let mapping = ctx.compute(&RepoMappingKey(id.repo)).await?;
    let ast = match bazel_starlark::parse_reporting_all(build_label, source.clone()) {
        Ok(ast) => ast,
        Err(errors) => return Ok(Prepared::Failed(failed_package(id, build_file, errors))),
    };
    let autoloads = ctx.compute(&crate::autoload::AutoloadSettingsKey).await?;
    let context = match autoloads.enabled() {
        true => crate::bzl::BzlContext::Build,
        false => crate::bzl::BzlContext::Plain,
    };
    let loaded = load_all(ctx, build_label, &bazel_starlark::loads(&ast), None, &mapping.0, context).await;
    let loaded = loaded.map_err(|error| crate::reworded(&error, |why| format!("error loading package '{}': {why}", display(id))))?;
    let repos = transitive_repos(id.repo, loaded.iter().map(|(_, module)| &**module));
    let mappings = mappings(ctx, &repos).await?;
    let builtins = ctx.compute(&crate::BuiltinsKey).await?;
    let globals = match (autoloads.enabled(), autoloads.in_build(id.repo)) {
        (false, _) => builtins.build.clone(),
        (true, with) => ctx.compute(&crate::autoload::AutoloadGlobalsKey { with }).await?.build.clone(),
    };
    let module = ctx.global::<crate::Repositories>()?.0.module(ctx, id.repo).await?;
    let globber = GraphGlobber {
        ctx: ctx.clone(),
        package: RootedPath::new(root, id.path),
        runtime: tokio::runtime::Handle::current(),
    };
    Ok(Prepared::Ready(ast, Box::new(Evaluation {
        id,
        build_file,
        build_label,
        source,
        mapping: Arc::new(mapping.0.clone()),
        loaded,
        repos,
        mappings,
        globals,
        context,
        module,
        globber,
    })))
}

/// A package whose BUILD file couldn't be parsed: it has no targets but the
/// BUILD file, and the errors.
fn failed_package(id: PackageId, build_file: Symbol, errors: Vec<bazel_packages::PackageError>) -> Package {
    let mut builder = PackageBuilder::new(id, build_file);
    builder.errors = errors;
    builder.finish()
}

/// The package's name as Bazel's messages give it.
pub fn display(id: PackageId) -> String {
    if id.repo.is_main() {
        return id.path.to_string();
    }
    format!("@@{}//{}", id.repo, id.path)
}

/// Evaluates `glob()` through [`Glob`] keys, blocking the BUILD file's
/// evaluation thread until the result is ready.
#[derive(Clone)]
struct GraphGlobber {
    ctx: Ctx,
    package: RootedPath,
    runtime: tokio::runtime::Handle,
}

impl Globber for GraphGlobber {
    fn glob(&self, include: &[String], exclude: &[String], exclude_directories: bool, allow_empty: bool) -> anyhow::Result<Vec<String>> {
        let parse = |patterns: &[String]| -> anyhow::Result<Vec<GlobPattern>> {
            patterns.iter().map(|p| GlobPattern::parse(p).map_err(anyhow::Error::from)).collect()
        };
        let key = Glob {
            package: self.package,
            include: parse(include)?,
            exclude: parse(exclude)?,
            exclude_directories,
        };
        let result = self.runtime.block_on(self.ctx.compute(&key))?;
        if !allow_empty {
            if let Some(&index) = result.empty_includes.first() {
                anyhow::bail!(
                    "glob pattern '{}' didn't match anything, but allow_empty is set to False (the default value of allow_empty can be set with --incompatible_disallow_empty_glob).",
                    include[index as usize]
                );
            }
        }
        Ok(result.matches.iter().map(PathId::to_string).collect())
    }
}

/// Where a package's BUILD file is on disk, or `None` if there's no such
/// package.
pub async fn build_file_path(ctx: &Ctx, id: PackageId) -> Result<Option<PathBuf>, SkyError> {
    let workspace = ctx.global::<bazel_fs::Workspace>()?;
    build_file_path_in(ctx, &workspace.root, id).await
}

/// Where a package's BUILD file is on disk, for the workspace at
/// `workspace`.
async fn build_file_path_in(compute: &impl Compute, workspace: &Path, id: PackageId) -> Result<Option<PathBuf>, SkyError> {
    let lookup = compute.compute(&PackageLookupKey(id)).await?;
    let PackageLookup::Found { root, build_file } = &*lookup else {
        return Ok(None);
    };
    Ok(Some(RootedPath::new(*root, id.path.join(*build_file)).resolve_in(workspace)))
}

/// Where a target was declared, as messages give it: the BUILD file's path,
/// with the line and column of the call that made it if it's a rule.
pub async fn target_location(ctx: &Ctx, label: Label) -> Option<String> {
    let workspace = ctx.global::<bazel_fs::Workspace>().ok()?;
    target_location_in(ctx, &workspace.root, label).await
}

/// Where a target was declared, for the workspace at `workspace`, from a
/// graph session or key.
pub async fn target_location_in(compute: &impl Compute, workspace: &Path, label: Label) -> Option<String> {
    let path = build_file_path_in(compute, workspace, label.package).await.ok()??;
    let package = compute.compute(&PackageKey(label.package)).await.ok()?;
    let rule = match package.0.target(label.name) {
        Some(bazel_packages::Target::Rule(rule)) => rule,
        _ => return Some(path.display().to_string()),
    };
    Some(format!("{}:{}:{}", path.display(), rule.location.line, rule.location.column))
}

/// What Bazel adds to the error for a target its package doesn't declare:
/// that a file or directory of that name exists, and to export it, or the
/// targets of the package whose names are like it.
pub async fn missing_target_suggestion(ctx: &Ctx, label: Label, package: &Package) -> String {
    let name = label.name.as_str();
    let Ok(path) = PathId::parse(name) else {
        return String::new();
    };
    if name == "*" {
        return String::new();
    }
    let build_file = package.build_file.package.path.join(package.build_file.name);
    match file_state(ctx, label.package, path).await {
        Some(FileState::Directory) => {
            format!("; however, a source directory of this name exists.  (Perhaps add 'exports_files([\"{name}\"])' to {build_file}, or define a filegroup?)")
        }
        Some(FileState::File { .. } | FileState::Other) => format!("; however, a source file of this name exists.  (Perhaps add 'exports_files([\"{name}\"])' to {build_file}?)"),
        _ => suggested_targets(name, package.targets.iter().map(|target| target.name().as_str())),
    }
}

/// What is at `path` in a package's directory, if the package is found.
async fn file_state(ctx: &Ctx, package: PackageId, path: PathId) -> Option<FileState> {
    let lookup = ctx.compute(&PackageLookupKey(package)).await.ok()?;
    let PackageLookup::Found { root, .. } = &*lookup else {
        return None;
    };
    let state = ctx.compute(&FileStateKey(RootedPath::new(*root, package.path.join_path(path)))).await.ok()?;
    Some(state.as_ref().clone())
}

/// ` (did you mean a, b, or c?)` for the targets whose names are closest
/// to `name`, as Bazel's `TargetSuggester` picks them.
fn suggested_targets<'a>(name: &str, targets: impl Iterator<Item = &'a str>) -> String {
    const MAX_DISTANCE: usize = 5;
    const MAX_SUGGESTED: usize = 10;
    let lower = name.to_lowercase();
    let max = MAX_DISTANCE.min(name.chars().count().div_ceil(2));
    let mut by_distance: std::collections::BTreeMap<usize, Vec<&str>> = std::collections::BTreeMap::new();
    for target in targets {
        if let Some(distance) = bazel_starlark::edit_distance(&lower, &target.to_lowercase(), max) {
            by_distance.entry(distance).or_default().push(target);
        }
    }
    let suggested: Vec<&str> = match (by_distance.get(&0), by_distance.get(&1)) {
        (Some(exact), _) => exact.iter().take(MAX_SUGGESTED).copied().collect(),
        (None, Some(close)) => close.iter().take(MAX_SUGGESTED).copied().collect(),
        (None, None) => by_distance.range(2..max).flat_map(|(_, names)| names.iter().copied()).take(MAX_SUGGESTED).collect(),
    };
    match suggested.as_slice() {
        [] => String::new(),
        [only] => format!(" (did you mean {only}?)"),
        [rest @ .., last] => format!(" (did you mean {}, or {last}?)", rest.join(", ")),
    }
}

/// The error for a target its package doesn't declare.
pub fn no_such_target(label: Label, build_file: &std::path::Path) -> String {
    format!(
        "no such target '{label}': target '{}' not declared in package '{}' defined by {}",
        label.name,
        display(label.package),
        build_file.display()
    )
}
