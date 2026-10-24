//! Resolving the dependency graph: which version of each module the build
//! uses, and the canonical names of their repositories.

use crate::config::BzlmodConfig;
use crate::fetch_repo::FetchSpecKey;
use crate::module_file::{self, BazelDep, IncludeReader, ModuleFile, Override};
use crate::registry::RegistryModuleKey;
use crate::repo_spec::{ArchiveSpec, RepoSpec};
use crate::version::Version;
use bazel_fs::{FileContentKey, Root, RootedPath, Workspace};
use bazel_label::{Label, LabelContext, PackageId, RepoMappingTable, RepoName};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Modules whose repositories keep their module name as their canonical
/// name, as Bazel's do.
pub const WELL_KNOWN_MODULES: [&str; 3] = ["bazel_tools", "platforms", "local_config_platform"];

/// The root module's MODULE.bazel, with the files it includes.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ModuleFileValue(pub ModuleFile);

encoded_value!(ModuleFileValue);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RootModuleKey;

impl Key for RootModuleKey {
    type Value = ModuleFileValue;
    const KIND: &'static str = "ROOT_MODULE";

    async fn compute(&self, ctx: &Ctx) -> Result<ModuleFileValue, SkyError> {
        let path = RootedPath::new(Root::Workspace, PathId::parse("MODULE.bazel").expect("a valid path"));
        let content = ctx.compute(&FileContentKey(path)).await?;
        let Some(bytes) = &content.bytes else {
            return Ok(ModuleFileValue(ModuleFile::default()));
        };
        let source = String::from_utf8_lossy(bytes).into_owned();
        let reader = Includes {
            ctx: ctx.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let file = tokio::task::spawn_blocking(move || module_file::evaluate("MODULE.bazel", &source, true, Some(&reader)))
            .await
            .map_err(|error| SkyError::transient(error.to_string()))?;
        file.map(ModuleFileValue).map_err(|error| SkyError::new(error.to_string()))
    }
}

/// Reads files the root module includes through the graph.
struct Includes {
    ctx: Ctx,
    runtime: tokio::runtime::Handle,
}

impl IncludeReader for Includes {
    fn read(&self, label: &str) -> anyhow::Result<String> {
        let mapping = RepoMappingTable::new(RepoName::MAIN, Vec::new());
        let context = LabelContext {
            package: PackageId::new(RepoName::MAIN, PathId::ROOT),
            mapping: &mapping,
        };
        let label = Label::parse(label, &context)?;
        if !label.repo().is_main() {
            anyhow::bail!("include() can only include files of the main repository");
        }
        let path = label.package.path.join_path(PathId::parse(label.name.as_str())?);
        let content = self.runtime.block_on(self.ctx.compute(&FileContentKey(RootedPath::new(Root::Workspace, path))))?;
        let Some(bytes) = &content.bytes else {
            anyhow::bail!("include(): {label} doesn't exist");
        };
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// A module the build uses.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ResolvedModule {
    pub name: String,
    pub version: String,
    pub repo: RepoName,
    /// The name the module's repository has for itself.
    pub self_name: String,
    /// Each dependency's name in this module, and its repository.
    pub deps: Vec<(String, RepoName)>,
    pub file: ModuleFile,
    /// Where the module's repository comes from; `None` for the root.
    pub spec: Option<RepoSpec>,
}

/// A module extension, and the modules that use it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ExtensionInfo {
    pub bzl: Label,
    pub name: String,
    /// The start of the canonical names of the repositories it generates.
    pub prefix: String,
    /// Each use, as (module index, usage index).
    pub users: Vec<(u32, u32)>,
    /// Whether this is an `isolate = True` use, which gets its own copy of
    /// the extension.
    pub isolated: bool,
}

/// The resolved dependency graph. The root module is first.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct DepGraph {
    pub modules: Vec<ResolvedModule>,
    pub extensions: Vec<ExtensionInfo>,
}

encoded_value!(DepGraph);

impl DepGraph {
    pub fn module(&self, repo: RepoName) -> Option<(usize, &ResolvedModule)> {
        self.modules.iter().enumerate().find(|(_, module)| module.repo == repo)
    }

    /// The extension that generated `repo`, and the name it gave it.
    pub fn extension_of(&self, repo: RepoName) -> Option<(usize, &str)> {
        let name = repo.as_str();
        let mut best: Option<(usize, &str)> = None;
        for (index, extension) in self.extensions.iter().enumerate() {
            let Some(rest) = name.strip_prefix(&extension.prefix).and_then(|rest| rest.strip_prefix('+')) else {
                continue;
            };
            if best.is_none_or(|(current, _)| self.extensions[current].prefix.len() < extension.prefix.len()) {
                best = Some((index, rest));
            }
        }
        best
    }

    /// The module whose `use_repo_rule` defined `repo`, and the call.
    pub fn repo_rule_of(&self, repo: RepoName) -> Option<(usize, usize)> {
        for (index, module) in self.modules.iter().enumerate() {
            let prefix = format!("{}+_repo_rules+", module.repo);
            let Some(name) = repo.as_str().strip_prefix(&prefix) else {
                continue;
            };
            if let Some(call) = module.file.repo_rules.iter().position(|call| call.name == name) {
                return Some((index, call));
            }
        }
        None
    }

    /// How a module's repository sees others.
    pub fn module_mapping(&self, index: usize) -> RepoMappingTable {
        let module = &self.modules[index];
        let mut entries = base_entries(module);
        for (usage_index, usage) in module.file.extension_usages.iter().enumerate() {
            let Some(extension) = self
                .extensions
                .iter()
                .find(|extension| extension.users.contains(&(index as u32, usage_index as u32)))
            else {
                continue;
            };
            for import in &usage.imports {
                let repo = RepoName::new(&format!("{}+{}", extension.prefix, import.exported));
                entries.push((Symbol::intern(&import.apparent), repo));
            }
        }
        for call in &module.file.repo_rules {
            let repo = RepoName::new(&format!("{}+_repo_rules+{}", module.repo, call.name));
            entries.push((Symbol::intern(&call.name), repo));
        }
        RepoMappingTable::new(module.repo, entries)
    }
}

/// A module's mapping before extensions: itself, its dependencies, and
/// Bazel's built-in repositories.
fn base_entries(module: &ResolvedModule) -> Vec<(Symbol, RepoName)> {
    let mut entries = vec![
        (Symbol::intern(&module.self_name), module.repo),
        (Symbol::intern(""), RepoName::MAIN),
        (Symbol::intern("bazel_tools"), RepoName::new("bazel_tools")),
        (Symbol::intern("local_config_platform"), RepoName::new("local_config_platform")),
    ];
    if module.repo.is_main() {
        entries.push((Symbol::intern(&module.name), RepoName::MAIN));
    }
    for (apparent, repo) in &module.deps {
        entries.push((Symbol::intern(apparent), *repo));
    }
    entries
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct DepGraphKey;

impl Key for DepGraphKey {
    type Value = DepGraph;
    const KIND: &'static str = "BAZEL_DEP_GRAPH";

    async fn compute(&self, ctx: &Ctx) -> Result<DepGraph, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        let root = ctx.compute(&RootModuleKey).await?.0.clone();
        let overrides: HashMap<String, Override> = root.overrides.iter().cloned().collect();
        let discovered = discover(ctx, &config, &root, &overrides).await?;
        let selected = select(&discovered, &overrides)?;
        let modules = reachable(&root, &discovered, &selected)?;
        let extensions = extensions(&modules)?;
        Ok(DepGraph { modules, extensions })
    }
}

/// A module version found while discovering the graph.
struct Discovered {
    file: ModuleFile,
    spec: Option<RepoSpec>,
}

/// The module version a dependency asks for, after the root's overrides.
fn requested_version(dep: &BazelDep, overrides: &HashMap<String, Override>) -> String {
    match overrides.get(&dep.name) {
        Some(Override::SingleVersion { version, .. }) if !version.is_empty() => version.clone(),
        Some(o) if o.is_non_registry() => String::new(),
        _ => dep.version.clone(),
    }
}

/// Every module version reachable from the root, before selection.
async fn discover(
    ctx: &Ctx,
    config: &BzlmodConfig,
    root: &ModuleFile,
    overrides: &HashMap<String, Override>,
) -> Result<HashMap<(String, String), Discovered>, SkyError> {
    let mut discovered = HashMap::new();
    let mut frontier: Vec<(String, String)> = root
        .deps
        .iter()
        .filter(|dep| dep.name != root.name)
        .map(|dep| (dep.name.clone(), requested_version(dep, overrides)))
        .collect();
    frontier.push(("bazel_tools".to_string(), String::new()));
    let mut seen: HashSet<(String, String)> = frontier.iter().cloned().collect();

    while !frontier.is_empty() {
        let mut registry_keys = Vec::new();
        let mut registry_ids = Vec::new();
        for (name, version) in std::mem::take(&mut frontier) {
            if let Some(found) = special_module(ctx, config, &name, overrides).await? {
                discovered.insert((name, version), found);
                continue;
            }
            let registries = match overrides.get(&name) {
                Some(Override::SingleVersion { registry, .. }) if !registry.is_empty() => vec![registry.clone()],
                _ => config.registries.clone(),
            };
            registry_keys.push(RegistryModuleKey {
                registries,
                name: name.clone(),
                version: version.clone(),
            });
            registry_ids.push((name, version));
        }
        for (id, module) in registry_ids.into_iter().zip(ctx.compute_many(&registry_keys).await) {
            let module = module.map_err(|error| error.context(format!("resolving {}@{}", id.0, id.1)))?;
            let mut spec = module.spec.clone();
            add_override_patches(&mut spec, overrides.get(&id.0));
            discovered.insert(
                id,
                Discovered {
                    file: module.file.clone(),
                    spec: Some(spec),
                },
            );
        }
        for found in discovered.values() {
            for dep in &found.file.deps {
                let id = (dep.name.clone(), requested_version(dep, overrides));
                if id.0 != root.name && seen.insert(id.clone()) {
                    frontier.push(id);
                }
            }
        }
    }
    Ok(discovered)
}

/// A single_version_override's patches apply on top of the registry's.
fn add_override_patches(spec: &mut RepoSpec, override_: Option<&Override>) {
    let (Some(Override::SingleVersion { patches, patch_strip, .. }), RepoSpec::Archive(archive)) = (override_, spec) else {
        return;
    };
    for patch in patches {
        archive.patches.push(crate::repo_spec::RemoteFile {
            url: format!("label:{patch}"),
            integrity: String::new(),
        });
    }
    if !patches.is_empty() {
        archive.patch_strip = *patch_strip;
    }
}

/// A module that doesn't come from a registry: built in, or overridden.
async fn special_module(
    ctx: &Ctx,
    config: &BzlmodConfig,
    name: &str,
    overrides: &HashMap<String, Override>,
) -> Result<Option<Discovered>, SkyError> {
    if name == "bazel_tools" {
        let Some(dir) = &config.bazel_tools else {
            return Err(SkyError::new("no @bazel_tools found; set BB_BAZEL_TOOLS"));
        };
        let spec = RepoSpec::Builtin {
            dir: dir.display().to_string(),
        };
        let file = read_module_file(ctx, Root::Dir(Symbol::intern(&dir.display().to_string())), "bazel_tools").await?;
        return Ok(Some(Discovered { file, spec: Some(spec) }));
    }
    let Some(override_) = overrides.get(name) else {
        return Ok(None);
    };
    let spec = match override_ {
        Override::LocalPath { path } => {
            let workspace = ctx.global::<Workspace>()?;
            RepoSpec::Local {
                path: workspace.root.join(path).display().to_string(),
            }
        }
        Override::Archive {
            urls,
            integrity,
            strip_prefix,
            patches,
            patch_strip,
            ..
        } => RepoSpec::Archive(ArchiveSpec {
            urls: urls.clone(),
            integrity: integrity.clone(),
            strip_prefix: strip_prefix.clone(),
            archive_type: String::new(),
            patches: patches
                .iter()
                .map(|patch| crate::repo_spec::RemoteFile {
                    url: format!("label:{patch}"),
                    integrity: String::new(),
                })
                .collect(),
            patch_strip: *patch_strip,
            overlay: Vec::new(),
            module_file: None,
        }),
        Override::Git { remote, .. } => return Err(SkyError::new(format!("git_override of {name} ({remote}) isn't supported yet"))),
        Override::SingleVersion { .. } | Override::MultipleVersion { .. } => return Ok(None),
    };
    let repo = canonical_name(name, "", false);
    let fetched = ctx.compute(&FetchSpecKey { repo, spec: spec.clone() }).await?;
    let file = read_module_file(ctx, Root::Dir(fetched.dir), name).await?;
    Ok(Some(Discovered { file, spec: Some(spec) }))
}

async fn read_module_file(ctx: &Ctx, root: Root, name: &str) -> Result<ModuleFile, SkyError> {
    let path = RootedPath::new(root, PathId::parse("MODULE.bazel").expect("a valid path"));
    let content = ctx.compute(&FileContentKey(path)).await?;
    let source = content.bytes.as_deref().map(String::from_utf8_lossy).unwrap_or_default().into_owned();
    let mut file = module_file::evaluate(&format!("@@{name}//:MODULE.bazel"), &source, false, None)
        .map_err(|error| SkyError::new(format!("{name}: {error}")))?;
    if file.name.is_empty() {
        file.name = name.to_string();
    }
    Ok(file)
}

/// Choose one version of each module: the highest any dependent asks for.
fn select(discovered: &HashMap<(String, String), Discovered>, overrides: &HashMap<String, Override>) -> Result<HashMap<String, String>, SkyError> {
    let mut selected: BTreeMap<String, (Version, String)> = BTreeMap::new();
    for (name, version) in discovered.keys() {
        if let Some(Override::MultipleVersion { .. }) = overrides.get(name) {
            return Err(SkyError::new(format!("multiple_version_override of {name} isn't supported yet")));
        }
        let parsed = Version::parse(version).map_err(|error| SkyError::new(format!("{name}: {error}")))?;
        let replace = selected.get(name).is_none_or(|(current, _)| parsed > *current);
        if replace {
            selected.insert(name.clone(), (parsed, version.clone()));
        }
    }
    Ok(selected.into_iter().map(|(name, (_, version))| (name, version)).collect())
}

pub fn canonical_name(name: &str, version: &str, multiple_versions: bool) -> RepoName {
    if WELL_KNOWN_MODULES.contains(&name) {
        return RepoName::new(name);
    }
    if multiple_versions {
        return RepoName::new(&format!("{name}+{version}"));
    }
    RepoName::new(&format!("{name}+"))
}

/// The modules reachable from the root through the selected versions, root
/// first, then in the order they're reached.
fn reachable(
    root: &ModuleFile,
    discovered: &HashMap<(String, String), Discovered>,
    selected: &HashMap<String, String>,
) -> Result<Vec<ResolvedModule>, SkyError> {
    let canonical = |name: &str| canonical_name(name, &selected[name], false);
    let resolve_deps = |file: &ModuleFile| -> Vec<(String, RepoName)> {
        let mut deps = Vec::new();
        for dep in &file.deps {
            let Some(repo_name) = &dep.repo_name else {
                continue;
            };
            let repo = if dep.name == root.name { RepoName::MAIN } else { canonical(&dep.name) };
            deps.push((repo_name.clone(), repo));
        }
        deps
    };

    let mut modules = vec![ResolvedModule {
        name: root.name.clone(),
        version: root.version.clone(),
        repo: RepoName::MAIN,
        self_name: root.repo_name.clone().unwrap_or_else(|| root.name.clone()),
        deps: resolve_deps(root),
        file: root.clone(),
        spec: None,
    }];
    let mut queue: Vec<String> = root.deps.iter().map(|dep| dep.name.clone()).collect();
    queue.push("bazel_tools".to_string());
    let mut seen: HashSet<String> = HashSet::from([root.name.clone()]);
    let mut index = 0;
    while index < queue.len() {
        let name = queue[index].clone();
        index += 1;
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(version) = selected.get(&name) else {
            return Err(SkyError::new(format!("no version of {name} was found")));
        };
        let found = &discovered[&(name.clone(), version.clone())];
        queue.extend(found.file.deps.iter().map(|dep| dep.name.clone()));
        modules.push(ResolvedModule {
            name: name.clone(),
            version: version.clone(),
            repo: canonical(&name),
            self_name: found.file.repo_name.clone().unwrap_or_else(|| name.clone()),
            deps: resolve_deps(&found.file),
            file: found.file.clone(),
            spec: found.spec.clone(),
        });
    }
    Ok(modules)
}

/// Every module extension used, with the canonical-name prefix of its
/// repositories.
fn extensions(modules: &[ResolvedModule]) -> Result<Vec<ExtensionInfo>, SkyError> {
    let mut extensions: Vec<ExtensionInfo> = Vec::new();
    for (module_index, module) in modules.iter().enumerate() {
        let mapping = RepoMappingTable::new(module.repo, base_entries(module));
        let context = LabelContext {
            package: PackageId::new(module.repo, PathId::ROOT),
            mapping: &mapping,
        };
        for (usage_index, usage) in module.file.extension_usages.iter().enumerate() {
            let bzl = Label::parse(&usage.bzl, &context)
                .map_err(|error| SkyError::new(format!("module {}: use_extension(): {error}", module.name)))?;
            let user = (module_index as u32, usage_index as u32);
            if usage.isolate {
                let prefix = format!("{}+{}+{}", bzl.repo(), usage.name, extensions.len());
                extensions.push(ExtensionInfo {
                    bzl,
                    name: usage.name.clone(),
                    prefix,
                    users: vec![user],
                    isolated: true,
                });
                continue;
            }
            match extensions.iter_mut().find(|e| e.bzl == bzl && e.name == usage.name && !e.isolated) {
                Some(extension) => extension.users.push(user),
                None => extensions.push(ExtensionInfo {
                    bzl,
                    name: usage.name.clone(),
                    prefix: format!("{}+{}", bzl.repo(), usage.name),
                    users: vec![user],
                    isolated: false,
                }),
            }
        }
    }
    Ok(extensions)
}
