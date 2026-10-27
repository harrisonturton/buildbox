//! Resolving the dependency graph: which version of each module the build
//! uses, and the canonical names of their repositories.

use crate::config::BzlmodConfig;
use crate::fetch_repo::FetchSpecKey;
use crate::module_file::{self, BazelDep, IncludeReader, ModuleFile, ModuleFileError, Override};
use crate::fetch_repo::read_label_patch;
use crate::registry::{self, Registries, RegistriesKey, RegistryFileKey, RegistryLookup, RegistryModule, RegistryModuleKey};
use crate::selection::{self, DepSpec, Interim, ModuleKey, MultipleVersions, Selection};
use crate::repo_spec::{ArchiveSpec, GitSpec, RepoSpec};
use bazel_fs::{Root, RootedPath, Workspace};
use bazel_label::{Label, LabelContext, PackageId, RepoMappingTable, RepoName};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::collections::{HashMap, HashSet};

/// Modules whose repositories keep their module name as their canonical
/// name, as Bazel's do.
pub const WELL_KNOWN_MODULES: [&str; 3] = ["bazel_tools", "platforms", "local_config_platform"];

/// The root module's MODULE.bazel, with the files it includes, or why it
/// couldn't be evaluated.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ModuleFileValue {
    pub file: ModuleFile,
    pub error: Option<ModuleFileError>,
}

encoded_value!(ModuleFileValue);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RootModuleKey;

impl Key for RootModuleKey {
    type Value = ModuleFileValue;
    const KIND: &'static str = "ROOT_MODULE";

    async fn compute(&self, ctx: &Ctx) -> Result<ModuleFileValue, SkyError> {
        let path = RootedPath::new(Root::Workspace, PathId::parse("MODULE.bazel").expect("a valid path"));
        let Some(bytes) = bazel_fs::read_file(ctx, path).await? else {
            return Ok(ModuleFileValue { file: ModuleFile::default(), error: None });
        };
        let source = String::from_utf8_lossy(&bytes).into_owned();
        let reader = Includes {
            ctx: ctx.clone(),
            runtime: tokio::runtime::Handle::current(),
        };
        let file = tokio::task::spawn_blocking(move || module_file::evaluate_root(&source, Some(&reader)))
            .await
            .map_err(|error| SkyError::transient(error.to_string()))?;
        match file {
            Ok(file) => Ok(ModuleFileValue { file, error: None }),
            Err(error) => Ok(ModuleFileValue { file: ModuleFile::default(), error: Some(error) }),
        }
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
            lenient: false,
        };
        let label = Label::parse(label, &context)?;
        if !label.repo().is_main() {
            anyhow::bail!("include() can only include files of the main repository");
        }
        let path = label.package.path.join_path(PathId::parse(label.name.as_str())?);
        let content = self.runtime.block_on(bazel_fs::read_file(&self.ctx, RootedPath::new(Root::Workspace, path)))?;
        let Some(bytes) = content else {
            anyhow::bail!("include(): {label} doesn't exist");
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
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
    /// The root module's dependencies that resolved to other versions
    /// than it asks for: each repository's name, the version asked for
    /// and the version resolved, which `--check_direct_dependencies`
    /// reports.
    pub direct_dep_mismatches: Vec<(String, String, String)>,
    /// The selected versions their registries have yanked, with why,
    /// which are errors unless `--allow_yanked_versions` allows them.
    pub yanked: Vec<(String, String)>,
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
            let names = repo_rule_repos(module);
            if let Some(call) = names.iter().position(|name| *name == repo) {
                return Some((index, call));
            }
        }
        None
    }

    /// How a module's repository sees others, in the order Bazel lists
    /// them: the repositories it imports from extensions and with
    /// `use_repo_rule`, then the main repository for the root, then
    /// itself, then its `bazel_dep`s.
    pub fn ordered_module_mapping(&self, index: usize) -> Vec<(String, RepoName)> {
        let module = &self.modules[index];
        let mut entries: Vec<(String, RepoName)> = Vec::new();
        let mut put = |name: &str, repo: RepoName| match entries.iter_mut().find(|(existing, _)| existing == name) {
            Some(entry) => entry.1 = repo,
            None => entries.push((name.to_string(), repo)),
        };
        for (usage_index, usage) in module.file.extension_usages.iter().enumerate() {
            let user = (index as u32, usage_index as u32);
            let Some(extension) = self.extensions.iter().find(|extension| extension.users.contains(&user)) else {
                continue;
            };
            for import in &usage.imports {
                put(&import.apparent, RepoName::new(&format!("{}+{}", extension.prefix, import.exported)));
            }
        }
        for (call, repo) in module.file.repo_rules.iter().zip(repo_rule_repos(module)) {
            put(&call.name, repo);
        }
        if module.repo.is_main() {
            put("", RepoName::MAIN);
        }
        put(&module.self_name, module.repo);
        for (apparent, repo) in &module.deps {
            put(apparent, *repo);
        }
        entries
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
        for (call, repo) in module.file.repo_rules.iter().zip(repo_rule_repos(module)) {
            entries.push((Symbol::intern(&call.name), repo));
        }
        RepoMappingTable::new(module.repo, entries)
    }
}

/// The repositories of a module's `use_repo_rule` calls, in order, named as
/// Bazel names them: `<module>+<rule>+<name>`, the rule's name numbered
/// from 2 when the module uses rules of that name from several files.
pub fn repo_rule_repos(module: &ResolvedModule) -> Vec<RepoName> {
    let mut rules: Vec<(&str, &str)> = Vec::new();
    let mut repos = Vec::with_capacity(module.file.repo_rules.len());
    for call in &module.file.repo_rules {
        let rule = (call.bzl.as_str(), call.rule.as_str());
        if !rules.contains(&rule) {
            rules.push(rule);
        }
        let earlier = rules.iter().take_while(|seen| **seen != rule).filter(|seen| seen.1 == rule.1).count();
        let disambiguator = match earlier {
            0 => String::new(),
            earlier => (earlier + 1).to_string(),
        };
        repos.push(RepoName::new(&format!("{}+{}{}+{}", module.repo, call.rule, disambiguator, call.name)));
    }
    repos
}

/// A module's mapping before extensions: itself, its dependencies, and
/// Bazel's built-in repositories.
fn base_entries(module: &ResolvedModule) -> Vec<(Symbol, RepoName)> {
    let mut entries = vec![
        (Symbol::intern(&module.self_name), module.repo),
        (Symbol::intern(""), RepoName::MAIN),
        (Symbol::intern("bazel_tools"), RepoName::new("bazel_tools")),
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
        let root = ctx.compute(&RootModuleKey).await?;
        if let Some(error) = &root.error {
            return Err(SkyError::new(error.summary.clone()));
        }
        let root = root.file.clone();
        let registries = (*ctx.compute(&RegistriesKey).await?).clone();
        let overrides: HashMap<String, Override> = root.overrides.iter().cloned().collect();
        let discovery = discover(ctx, &config, &registries, &root, &overrides).await?;
        check_overrides_exist(&root, &discovery)?;
        let graph = interim_graph(&root, &discovery, &overrides);
        let selection = selection::select(&graph, &discovery.order, &multiple_versions(&overrides)).map_err(SkyError::new)?;
        let modules = resolved_modules(&root, &discovery, &selection);
        let direct_dep_mismatches = direct_dep_mismatches(&graph, &selection);
        let yanked = yanked_versions(ctx, &discovery, &selection).await?;
        let extensions = extensions(&modules)?;
        Ok(DepGraph {
            modules,
            extensions,
            direct_dep_mismatches,
            yanked,
        })
    }
}

/// A module version found while discovering the graph.
struct Discovered {
    file: ModuleFile,
    spec: Option<RepoSpec>,
    /// The registry it came from, if it came from one.
    registry: Option<String>,
}

/// The module versions reachable from the root, before selection, in the
/// order they were found, the root first.
struct Discovery {
    modules: HashMap<ModuleKey, Discovered>,
    order: Vec<ModuleKey>,
}

/// The module version a dependency asks for, after the root's overrides:
/// the root itself, an overridden version, or no version for a module
/// that doesn't come from a registry.
fn requested(dep: &BazelDep, root: &ModuleFile, overrides: &HashMap<String, Override>) -> ModuleKey {
    if dep.name == root.name {
        return ModuleKey::root();
    }
    let version = match overrides.get(&dep.name) {
        Some(Override::SingleVersion { version, .. }) if !version.is_empty() => version.clone(),
        Some(o) if o.is_non_registry() => String::new(),
        _ => dep.version.clone(),
    };
    ModuleKey { name: dep.name.clone(), version }
}

/// A module's dependencies, with the `bazel_tools` every module but it
/// depends on.
fn deps_of(file: &ModuleFile) -> Vec<BazelDep> {
    let mut deps: Vec<BazelDep> = file.deps.iter().filter(|dep| dep.repo_name.as_deref() != Some("bazel_tools")).cloned().collect();
    if file.name != "bazel_tools" {
        deps.push(BazelDep {
            name: "bazel_tools".to_string(),
            version: String::new(),
            max_compatibility_level: -1,
            repo_name: Some("bazel_tools".to_string()),
            dev: false,
        });
    }
    deps
}

/// Every module version reachable from the root, as Bazel discovers them:
/// in rounds, each following the dependencies that only constrain
/// selection to modules the round before found, until a round finds no
/// more of those.
async fn discover(
    ctx: &Ctx,
    config: &BzlmodConfig,
    registries: &Registries,
    root: &ModuleFile,
    overrides: &HashMap<String, Override>,
) -> Result<Discovery, SkyError> {
    let mut known_names: HashSet<String> = HashSet::from([root.name.clone()]);
    loop {
        let (discovery, unfulfilled) = discovery_round(ctx, config, registries, root, overrides, &known_names).await?;
        let names: HashSet<String> = discovery.order.iter().map(|key| key.name.clone()).collect();
        if !unfulfilled.iter().any(|name| names.contains(name)) {
            return Ok(discovery);
        }
        known_names = names;
    }
}

/// One round of discovery, which follows dependencies that only constrain
/// selection to the `known` modules, returning the names of those it
/// didn't follow.
async fn discovery_round(
    ctx: &Ctx,
    config: &BzlmodConfig,
    registries: &Registries,
    root: &ModuleFile,
    overrides: &HashMap<String, Override>,
    known: &HashSet<String>,
) -> Result<(Discovery, HashSet<String>), SkyError> {
    let mut modules = HashMap::from([(ModuleKey::root(), Discovered { file: root.clone(), spec: None, registry: None })]);
    let mut order = vec![ModuleKey::root()];
    let mut parents: HashMap<ModuleKey, ModuleKey> = HashMap::new();
    let mut unfulfilled = HashSet::new();
    let mut horizon = vec![ModuleKey::root()];
    while !horizon.is_empty() {
        let mut next = Vec::new();
        for key in &horizon {
            for dep in deps_of(&modules[key].file) {
                let dep_key = requested(&dep, root, overrides);
                if dep.repo_name.is_none() && !known.contains(&dep.name) {
                    unfulfilled.insert(dep.name.clone());
                    continue;
                }
                if modules.contains_key(&dep_key) || next.contains(&dep_key) {
                    continue;
                }
                parents.entry(dep_key.clone()).or_insert_with(|| key.clone());
                next.push(dep_key);
            }
        }
        for (key, found) in next.iter().zip(fetch_modules(ctx, config, registries, overrides, &next, &parents).await?) {
            modules.insert(key.clone(), found);
            order.push(key.clone());
        }
        horizon = next;
    }
    Ok((Discovery { modules, order }, unfulfilled))
}

/// The MODULE.bazel files and specs of module versions, from overrides or
/// the registries.
async fn fetch_modules(
    ctx: &Ctx,
    config: &BzlmodConfig,
    registries: &Registries,
    overrides: &HashMap<String, Override>,
    keys: &[ModuleKey],
    parents: &HashMap<ModuleKey, ModuleKey>,
) -> Result<Vec<Discovered>, SkyError> {
    let mut found: Vec<Option<Discovered>> = Vec::with_capacity(keys.len());
    let mut registry_keys = Vec::new();
    for key in keys {
        let special = special_module(ctx, config, &key.name, overrides).await?;
        if special.is_none() {
            let selected_registries = match overrides.get(&key.name) {
                Some(Override::SingleVersion { registry, .. }) if !registry.is_empty() => vec![registry.clone()],
                _ => registries.registries.clone(),
            };
            registry_keys.push(RegistryModuleKey {
                registries: selected_registries,
                mirrors: registries.mirrors.clone(),
                name: key.name.clone(),
                version: key.version.clone(),
            });
        }
        found.push(special);
    }
    let mut looked_up = ctx.compute_many(&registry_keys).await.into_iter();
    let mut modules = Vec::with_capacity(keys.len());
    for (key, special) in keys.iter().zip(found) {
        if let Some(special) = special {
            modules.push(special);
            continue;
        }
        let lookup = looked_up.next().expect("a lookup for each registry module")?;
        let module = match &*lookup {
            RegistryLookup::Found(module) => module.clone(),
            RegistryLookup::Missing(missing) => {
                let chain = dependency_chain(parents, key);
                return Err(SkyError::new(format!("in module dependency chain {chain}: module {key} not found in registries:{missing}")));
            }
        };
        let mut spec = module.spec.clone();
        add_override_patches(&mut spec, overrides.get(&key.name));
        let id = (key.name.clone(), key.version.clone());
        let file = patched_module_file(ctx, &module, &id, overrides.get(&key.name)).await?;
        modules.push(Discovered {
            file,
            spec: Some(spec),
            registry: Some(module.registry.clone()),
        });
    }
    Ok(modules)
}

/// How the root module comes to depend on a module, as Bazel shows it:
/// `<root> -> a@1.0 -> b@2.0`.
fn dependency_chain(parents: &HashMap<ModuleKey, ModuleKey>, key: &ModuleKey) -> String {
    let mut chain = vec![key.to_string()];
    let mut current = key;
    while let Some(parent) = parents.get(current) {
        chain.push(parent.to_string());
        current = parent;
    }
    chain.reverse();
    chain.join(" -> ")
}

/// An error if the root module overrides a module the graph doesn't have.
fn check_overrides_exist(root: &ModuleFile, discovery: &Discovery) -> Result<(), SkyError> {
    let names: HashSet<&str> = discovery.order.iter().map(|key| key.name.as_str()).collect();
    let missing: Vec<&str> = root.overrides.iter().map(|(name, _)| name.as_str()).filter(|name| !names.contains(name)).collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(SkyError::new(format!("the root module specifies overrides on nonexistent module(s): {}", missing.join(", "))))
}

/// The graph selection works on: each module version's compatibility
/// level and dependencies, those that only constrain selection kept only
/// if discovery found them.
fn interim_graph(root: &ModuleFile, discovery: &Discovery, overrides: &HashMap<String, Override>) -> HashMap<ModuleKey, Interim> {
    let mut graph = HashMap::new();
    for key in &discovery.order {
        let file = &discovery.modules[key].file;
        let mut interim = Interim {
            compatibility_level: file.compatibility_level,
            deps: Vec::new(),
            nodep_deps: Vec::new(),
        };
        for dep in deps_of(file) {
            let requested = requested(&dep, root, overrides);
            let spec = DepSpec {
                name: requested.name.clone(),
                version: requested.version.clone(),
                max_compatibility_level: dep.max_compatibility_level,
            };
            match &dep.repo_name {
                Some(repo_name) => interim.deps.push((repo_name.clone(), spec)),
                None if discovery.modules.contains_key(&requested) => interim.nodep_deps.push(spec),
                None => {}
            }
        }
        graph.insert(key.clone(), interim);
    }
    graph
}

/// The versions each `multiple_version_override` allows.
fn multiple_versions(overrides: &HashMap<String, Override>) -> MultipleVersions {
    let mut multiple = MultipleVersions::new();
    for (name, override_) in overrides {
        if let Override::MultipleVersion { versions, .. } = override_ {
            multiple.insert(name.clone(), versions.clone());
        }
    }
    multiple
}

/// The selected modules, root first, as the graph's repositories: each
/// named `name+`, or `name+version` for a module kept at several versions.
fn resolved_modules(root: &ModuleFile, discovery: &Discovery, selection: &Selection) -> Vec<ResolvedModule> {
    let mut versions: HashMap<&str, usize> = HashMap::new();
    for module in &selection.modules {
        *versions.entry(module.key.name.as_str()).or_default() += 1;
    }
    let repo_of = |key: &ModuleKey| match key.name.is_empty() {
        true => RepoName::MAIN,
        false => canonical_name(&key.name, &key.version, versions[key.name.as_str()] > 1),
    };
    let mut modules = Vec::with_capacity(selection.modules.len());
    for selected in &selection.modules {
        let found = &discovery.modules[&selected.key];
        let name = match selected.key.name.is_empty() {
            true => root.name.clone(),
            false => selected.key.name.clone(),
        };
        modules.push(ResolvedModule {
            self_name: found.file.repo_name.clone().unwrap_or_else(|| name.clone()),
            name,
            version: match selected.key.name.is_empty() {
                true => root.version.clone(),
                false => selected.key.version.clone(),
            },
            repo: repo_of(&selected.key),
            deps: selected.deps.iter().map(|(apparent, dep)| (apparent.clone(), repo_of(dep))).collect(),
            file: found.file.clone(),
            spec: found.spec.clone(),
        });
    }
    modules
}

/// The root module's dependencies that resolved to other versions than it
/// asks for: each repository's name, the version asked for and the
/// version resolved, as `name@version`.
fn direct_dep_mismatches(graph: &HashMap<ModuleKey, Interim>, selection: &Selection) -> Vec<(String, String, String)> {
    let mut mismatches = Vec::new();
    for (repo_name, spec) in &graph[&ModuleKey::root()].deps {
        let Some(resolved) = selection.resolved(spec) else {
            continue;
        };
        if resolved != spec.version {
            let wanted = ModuleKey { name: spec.name.clone(), version: spec.version.clone() };
            let got = ModuleKey { name: spec.name.clone(), version: resolved.to_string() };
            mismatches.push((repo_name.clone(), wanted.to_string(), got.to_string()));
        }
    }
    mismatches
}

/// The selected versions their registries have yanked, as `name@version`,
/// with why.
async fn yanked_versions(ctx: &Ctx, discovery: &Discovery, selection: &Selection) -> Result<Vec<(String, String)>, SkyError> {
    let mut yanked = Vec::new();
    for selected in &selection.modules {
        let Some(registry) = &discovery.modules[&selected.key].registry else {
            continue;
        };
        let reasons = ctx.compute(&registry::YankedVersionsKey { registry: registry.clone(), name: selected.key.name.clone() }).await?;
        if let Some((_, reason)) = reasons.0.iter().find(|(version, _)| *version == selected.key.version) {
            yanked.push((selected.key.to_string(), reason.clone()));
        }
    }
    Ok(yanked)
}

/// A registry module's MODULE.bazel, with the changes a
/// single_version_override's patches make to it, as Bazel applies them
/// during resolution. Only patches in the main repository apply, since
/// other repositories can't be fetched before resolution.
async fn patched_module_file(ctx: &Ctx, module: &RegistryModule, id: &(String, String), override_: Option<&Override>) -> Result<ModuleFile, SkyError> {
    let Some(Override::SingleVersion { patches, patch_strip, .. }) = override_ else {
        return Ok(module.file.clone());
    };
    let patches: Vec<&String> = patches.iter().filter(|patch| in_main_repo(patch)).collect();
    if patches.is_empty() {
        return Ok(module.file.clone());
    }
    let url = registry::module_file_url(&module.registry, &id.0, &id.1);
    let registry_file = ctx.compute(&RegistryFileKey(url.clone())).await?;
    let Some(bytes) = &registry_file.0 else {
        return Err(SkyError::new(format!("{url} no longer exists")));
    };
    let mut source = String::from_utf8_lossy(bytes).into_owned();
    for patch in patches {
        let contents = read_label_patch(ctx, patch).await?;
        let contents = String::from_utf8_lossy(&contents);
        source = bazel_fetch::apply_to_file(&source, "MODULE.bazel", &contents, *patch_strip as usize, patch)
            .map_err(|error| SkyError::new(format!("error applying single_version_override patch {patch} to module file: {error}")))?;
    }
    module_file::evaluate(&url, &source, false, None).map_err(|error| SkyError::new(format!("{url}, patched: {error}")))
}

/// Whether `label`, as the root module writes it, is in the main repository.
fn in_main_repo(label: &str) -> bool {
    label.starts_with("//") || label.starts_with("@//") || label.starts_with("@@//")
}

/// A single_version_override's patches and commands apply on top of the
/// registry's patches.
fn add_override_patches(spec: &mut RepoSpec, override_: Option<&Override>) {
    let Some(Override::SingleVersion { patches, patch_cmds, patch_strip, .. }) = override_ else {
        return;
    };
    match spec {
        RepoSpec::Archive(archive) => {
            archive.local_patches = patches.clone();
            archive.local_patch_strip = *patch_strip;
            archive.patch_cmds = patch_cmds.clone();
        }
        RepoSpec::Git(git) => {
            git.source.patches = patches.clone();
            git.source.patch_strip = *patch_strip;
            git.source.patch_cmds = patch_cmds.clone();
        }
        _ => {}
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
        return Ok(Some(Discovered { file, spec: Some(spec), registry: None }));
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
            archive_type,
            patches,
            patch_cmds,
            patch_strip,
        } => RepoSpec::Archive(ArchiveSpec {
            urls: urls.clone(),
            integrity: integrity.clone(),
            strip_prefix: strip_prefix.clone(),
            archive_type: archive_type.clone(),
            patches: Vec::new(),
            patch_strip: 0,
            local_patches: patches.clone(),
            local_patch_strip: *patch_strip,
            patch_cmds: patch_cmds.clone(),
            overlay: Vec::new(),
            module_file: None,
        }),
        Override::Git(source) => RepoSpec::Git(GitSpec {
            source: source.clone(),
            registry_patches: Vec::new(),
            registry_patch_strip: 0,
            module_file: None,
        }),
        Override::SingleVersion { .. } | Override::MultipleVersion { .. } => return Ok(None),
    };
    let repo = canonical_name(name, "", false);
    let fetched = ctx.compute(&FetchSpecKey { repo, spec: spec.clone() }).await;
    let fetched = fetched.map_err(|error| error.context("error during computation of main repo mapping"))?;
    let file = read_module_file(ctx, Root::Dir(fetched.dir), name).await?;
    Ok(Some(Discovered { file, spec: Some(spec), registry: None }))
}

async fn read_module_file(ctx: &Ctx, root: Root, name: &str) -> Result<ModuleFile, SkyError> {
    let path = RootedPath::new(root, PathId::parse("MODULE.bazel").expect("a valid path"));
    let content = bazel_fs::read_file(ctx, path).await?;
    let source = content.as_deref().map(String::from_utf8_lossy).unwrap_or_default().into_owned();
    let mut file = module_file::evaluate(&format!("@@{name}//:MODULE.bazel"), &source, false, None)
        .map_err(|error| SkyError::new(format!("{name}: {error}")))?;
    if file.name.is_empty() {
        file.name = name.to_string();
    }
    Ok(file)
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

/// `prefix`, or, if another extension of the same name in the same
/// repository already has it, `prefix2`, `prefix3` and so on, in order of
/// first use, as Bazel names them.
fn unique_prefix(extensions: &[ExtensionInfo], prefix: &str) -> String {
    if !extensions.iter().any(|extension| extension.prefix == prefix) {
        return prefix.to_string();
    }
    (2..)
        .map(|counter| format!("{prefix}{counter}"))
        .find(|candidate| !extensions.iter().any(|extension| &extension.prefix == candidate))
        .expect("some counter is free")
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
            lenient: false,
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
                None => {
                    let prefix = unique_prefix(&extensions, &format!("{}+{}", bzl.repo(), usage.name));
                    extensions.push(ExtensionInfo {
                        bzl,
                        name: usage.name.clone(),
                        prefix,
                        users: vec![user],
                        isolated: false,
                    });
                }
            }
        }
    }
    Ok(extensions)
}
