//! Loading packages from BUILD files.

use crate::bzl::{load_all, mappings, transitive_repos};
use crate::{RepoMappingKey, RepoRootKey, blocking};
use bazel_fs::{FileState, FileStateKey, Glob, GlobPattern, Root, RootedPath};
use bazel_label::{Label, PackageId, RepoMapping, RepoName};
use starlark::values::OwnedFrozenValue;
use bazel_packages::Package;
use bazel_starlark::{EvalContext, FileKind, Globber, Loads, PackageBuilder};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};

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
        let id = self.0;
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
        let ast = match bazel_starlark::parse(build_label, source.clone()) {
            Ok(ast) => ast,
            Err(error) => return Ok(LoadedPackage(failed_package(id, build_file, error.message))),
        };
        let loaded = load_all(ctx, build_label, &bazel_starlark::loads(&ast), &mapping.0).await?;
        let predeclared = autoloads(ctx, &source, &bazel_starlark::loaded_names(&ast)).await?;
        let repos = transitive_repos(id.repo, loaded.iter().map(|(_, module)| &**module));
        let mappings = mappings(ctx, &repos).await?;
        let load_labels = loaded.iter().map(|(_, module)| module.label).collect();
        let builtins = ctx.compute(&crate::BuiltinsKey).await?;
        let module = ctx.global::<crate::Repositories>()?.0.module(ctx, id.repo).await?;
        let globber = GraphGlobber {
            ctx: ctx.clone(),
            package: RootedPath::new(root, id.path),
            runtime: tokio::runtime::Handle::current(),
        };

        let (mut package, printed) = blocking(move || {
            let loads = Loads {
                modules: loaded.iter().map(|(load, module)| (load.clone(), &module.module)).collect(),
            };
            let mut context = EvalContext::new(FileKind::Build, build_label);
            context.mappings = mappings;
            context.globber = Some(Box::new(globber));
            context.module = module;
            let builder = PackageBuilder::new(id, build_file);
            bazel_starlark::eval_build(ast, &builtins.build, &loads, &predeclared, context, builder)
        })
        .await?;
        for line in printed {
            tracing::info!("DEBUG: {build_label}: {line}");
        }
        package.loads = load_labels;
        Ok(LoadedPackage(package))
    }
}

/// The autoloaded rules a BUILD file uses without loading them, resolved
/// as `@bazel_tools` sees their rule sets. Rule sets the build doesn't
/// have are skipped, leaving the name undefined, as in Bazel.
async fn autoloads(ctx: &Ctx, source: &str, loaded: &[String]) -> Result<Vec<(String, OwnedFrozenValue)>, SkyError> {
    let used = bazel_starlark::autoload::used_autoloads(source, loaded);
    if used.is_empty() {
        return Ok(Vec::new());
    }
    let bazel_tools = ctx.compute(&RepoMappingKey(RepoName::new("bazel_tools"))).await?;
    let mut keys = Vec::new();
    let mut symbols = Vec::new();
    for (symbol, repo, file) in used {
        let Some(repo) = bazel_tools.0.resolve(repo) else {
            continue;
        };
        let Ok(label) = Label::parse_canonical(&format!("@@{repo}{file}")) else {
            continue;
        };
        keys.push(crate::BzlLoadKey(label));
        symbols.push(*symbol);
    }
    let mut predeclared = Vec::new();
    for (symbol, module) in symbols.into_iter().zip(ctx.compute_many(&keys).await) {
        let module = module?;
        if let Ok(value) = module.module.get(symbol) {
            predeclared.push((symbol.to_string(), value));
        }
    }
    Ok(predeclared)
}

/// A package whose BUILD file couldn't be parsed: it has no targets but the
/// BUILD file, and the error.
fn failed_package(id: PackageId, build_file: Symbol, error: String) -> Package {
    let mut builder = PackageBuilder::new(id, build_file);
    builder.errors.push(error);
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
