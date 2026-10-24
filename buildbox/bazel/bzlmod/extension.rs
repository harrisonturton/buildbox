//! Evaluating module extensions.

use crate::attrs::{AttrContext, apply_schema, from_recorded};
use crate::config::BzlmodConfig;
use crate::graph_access::GraphAccess;
use crate::io::IoState;
use crate::module_ctx::{ModuleCtx, Tag};
use crate::repo_spec::RepoRuleSpec;
use crate::resolve::{DepGraph, DepGraphKey};
use bazel_label::{RepoMappingTable, RepoName};
use bazel_loading::{BzlLoadKey, RepoMappingKey};
use bazel_starlark::repo_api::{ModuleExtension, TagClass};
use bazel_starlark::{EvalContext, FileKind};
use intern::Symbol;
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::list::AllocList;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// The repositories a module extension generates, by the names it gave
/// them.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ExtensionResult {
    pub repos: Vec<(String, RepoRuleSpec)>,
}

encoded_value!(ExtensionResult);

/// Evaluates the extension at an index of the dependency graph.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ExtensionKey(pub u32);

impl Key for ExtensionKey {
    type Value = ExtensionResult;
    const KIND: &'static str = "MODULE_EXTENSION";

    async fn compute(&self, ctx: &Ctx) -> Result<ExtensionResult, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        let graph = ctx.compute(&DepGraphKey).await?;
        let Some(info) = graph.extensions.get(self.0 as usize).cloned() else {
            return Err(SkyError::new("no such module extension"));
        };
        let module = ctx.compute(&BzlLoadKey(info.bzl)).await?;
        crate::repo_env::declared(ctx, &extension_environ(&module, &info.name)).await?;
        let mut repos: Vec<RepoName> = module.repos.clone();
        repos.push(info.bzl.repo());
        repos.sort();
        repos.dedup();
        let keys: Vec<RepoMappingKey> = repos.iter().map(|repo| RepoMappingKey(*repo)).collect();
        let mut mappings = HashMap::new();
        for (repo, mapping) in repos.into_iter().zip(ctx.compute_many(&keys).await) {
            mappings.insert(repo, Arc::new(mapping?.0.clone()));
        }
        let work_dir = config.repos_dir.join(".extensions").join(format!("{}-{}", info.prefix, Fingerprint::of(info.prefix.as_bytes())));
        let io = IoState {
            base_dir: work_dir.clone(),
            graph: GraphAccess::new(ctx.clone()),
            downloads: config.downloads.clone(),
            env: ctx.global::<crate::repo_env::ClientEnv>()?.current().env.iter().cloned().collect(),
        };
        let span = tracing::info_span!("bzlmod.module_extension", extension = %info.prefix);
        let graph = graph.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let _ = std::fs::remove_dir_all(&work_dir);
            std::fs::create_dir_all(&work_dir).map_err(|error| error.to_string())?;
            let result = run_extension(&graph, self_index(&info, &graph), &module, io, mappings);
            let _ = std::fs::remove_dir_all(&work_dir);
            result.map_err(|error| format!("module extension {} in {}: {error}", info.name, info.bzl))
        })
        .await;
        let repos = result.map_err(|error| SkyError::transient(error.to_string()))?.map_err(SkyError::new)?;
        Ok(ExtensionResult { repos })
    }
}

fn self_index(info: &crate::resolve::ExtensionInfo, graph: &DepGraph) -> usize {
    graph.extensions.iter().position(|e| e == info).unwrap_or(0)
}

/// Run an extension's implementation and collect the repositories it
/// defines.
fn run_extension(
    graph: &DepGraph,
    index: usize,
    module: &bazel_loading::BzlModule,
    io: IoState,
    mappings: HashMap<RepoName, Arc<RepoMappingTable>>,
) -> anyhow::Result<Vec<(String, RepoRuleSpec)>> {
    let info = &graph.extensions[index];
    let owner = info.bzl.repo();
    let mut context = EvalContext::new(FileKind::Bzl, info.bzl);
    context.mappings = mappings;
    context.repos = Some(RefCell::new(Vec::new()));
    let graph_access = io.graph.clone();

    let result = Module::with_temp_heap(|env| -> anyhow::Result<()> {
        let heap = env.heap();
        let (value, _) = module
            .module
            .get_any_visibility(&info.name)
            .map_err(|_| anyhow::anyhow!("{} doesn't export '{}'", info.bzl, info.name))?;
        let value = heap.access_owned_frozen_value(&value);
        let Some(extension) = ModuleExtension::from_value(value) else {
            anyhow::bail!("{}%{} isn't a module extension", info.bzl, info.name);
        };
        let modules = module_values(graph, index, &extension.tag_classes, heap)?;
        let root_non_dev = info.users.iter().any(|(module, usage)| {
            *module == 0 && !graph.modules[0].file.extension_usages[*usage as usize].dev_only
        });
        let ctx = heap.alloc(ModuleCtx {
            modules,
            io,
            root_module_has_non_dev_dependency: root_non_dev,
        });
        let mut eval = Evaluator::new(&env);
        eval.extra = Some(&context);
        eval.eval_function(extension.implementation, &[ctx], &[]).map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    });
    graph_access.report_printed(&context.printed.take());
    result?;

    let definitions = context.repos.take().map(RefCell::into_inner).unwrap_or_default();
    Ok(definitions
        .into_iter()
        .map(|definition| {
            let spec = RepoRuleSpec {
                bzl: definition.rule.0,
                rule: definition.rule.1,
                attrs: definition.attrs.iter().map(|(name, value)| (name.clone(), from_recorded(value))).collect(),
                base_repo: owner,
                original_name: definition.name.clone(),
            };
            (definition.name, spec)
        })
        .collect())
}

/// `module_ctx.modules`: each module using the extension, with its tags.
fn module_values<'v>(
    graph: &DepGraph,
    index: usize,
    tag_classes: &[(String, Value<'v>)],
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    let info = &graph.extensions[index];
    let mut by_module: Vec<(usize, Vec<usize>)> = Vec::new();
    for (module, usage) in &info.users {
        match by_module.iter_mut().find(|(m, _)| *m == *module as usize) {
            Some((_, usages)) => usages.push(*usage as usize),
            None => by_module.push((*module as usize, vec![*usage as usize])),
        }
    }
    by_module.sort_by_key(|(module, _)| *module);

    let owner_mapping = match graph.module(info.bzl.repo()) {
        Some((owner, _)) => graph.module_mapping(owner),
        None => RepoMappingTable::new(info.bzl.repo(), Vec::new()),
    };
    // Defaults are relative to the file defining the tag classes.
    let default_context = AttrContext {
        package: info.bzl.package,
        mapping: &owner_mapping,
    };
    let mut values = Vec::with_capacity(by_module.len());
    for (module_index, usages) in by_module {
        let module = &graph.modules[module_index];
        let mapping = graph.module_mapping(module_index);
        let attr_context = AttrContext::repo_root(module.repo, &mapping);
        let mut tags_by_class: Vec<(&str, Vec<Value<'v>>)> = tag_classes.iter().map(|(name, _)| (name.as_str(), Vec::new())).collect();
        for usage in usages {
            for tag in &module.file.extension_usages[usage].tags {
                let Some(position) = tag_classes.iter().position(|(name, _)| *name == tag.class) else {
                    anyhow::bail!("{}: the extension has no tag class '{}'", tag.location, tag.class);
                };
                let Some(class) = TagClass::from_value(tag_classes[position].1) else {
                    anyhow::bail!("tag class '{}' isn't a tag_class() value", tag.class);
                };
                let what = format!("{}: tag {}", tag.location, tag.class);
                let attrs = apply_schema(&what, &class.attrs, &tag.attrs, &attr_context, &default_context)?;
                let mut fields = SmallMap::with_capacity(attrs.len());
                for (name, value) in attrs {
                    fields.insert(name, value.to_value(heap));
                }
                tags_by_class[position].1.push(heap.alloc(Tag { attrs: fields, dev: tag.dev }));
            }
        }
        let tags = heap.alloc(AllocStruct(
            tags_by_class
                .into_iter()
                .map(|(name, tags)| (name, heap.alloc(AllocList(tags)))),
        ));
        values.push(heap.alloc(AllocStruct([
            ("name", heap.alloc(module.name.as_str())),
            ("version", heap.alloc(module.version.as_str())),
            ("is_root", heap.alloc(module_index == 0)),
            ("tags", tags),
        ])));
    }
    Ok(heap.alloc(AllocList(values)))
}

/// How a repository an extension generated sees others: as the module
/// defining the extension does, plus the extension's other repositories.
pub async fn extension_mapping(ctx: &Ctx, graph: &DepGraph, extension: usize, repo: RepoName) -> Result<RepoMappingTable, SkyError> {
    let info = &graph.extensions[extension];
    let Some((owner, _)) = graph.module(info.bzl.repo()) else {
        return Err(SkyError::new(format!("module extension {} isn't defined in a module's repository", info.name)));
    };
    let base = graph.module_mapping(owner);
    let result = ctx.compute(&ExtensionKey(extension as u32)).await?;
    let mut entries = base.entries.clone();
    for (name, _) in &result.repos {
        entries.push((Symbol::intern(name), RepoName::new(&format!("{}+{name}", info.prefix))));
    }
    Ok(RepoMappingTable::new(repo, entries))
}

/// The variables a module extension declares, with `environ`, that it reads.
fn extension_environ(module: &bazel_loading::BzlModule, name: &str) -> Vec<String> {
    let Ok((value, _)) = module.module.get_any_visibility(name) else {
        return Vec::new();
    };
    ModuleExtension::from_value(value.value()).map(|extension| extension.environ.clone()).unwrap_or_default()
}
