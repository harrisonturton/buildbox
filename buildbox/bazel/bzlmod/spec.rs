//! Finding the spec of any repository.

use crate::command_overrides::{CommandOverrides, CommandOverridesKey};
use crate::repo_spec::{RepoRuleSpec, RepoSpec};
use crate::resolve::{DepGraph, DepGraphKey};
use bazel_label::{Label, LabelContext, PackageId, RepoMapping, RepoName};
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError};

/// The directory replacing `repo`, if an override names it by the apparent
/// name the main repository sees it by, or Bazel's error for an override
/// of an apparent name the main repository doesn't have.
fn overridden_by_apparent_name<'a>(graph: &DepGraph, command: &'a CommandOverrides, repo: RepoName) -> Result<Option<&'a str>, SkyError> {
    let mut apparent = command.by_apparent_name().peekable();
    if apparent.peek().is_none() {
        return Ok(None);
    }
    let Some((root, _)) = graph.module(RepoName::MAIN) else {
        return Ok(None);
    };
    let mapping = graph.module_mapping(root);
    for (name, path) in apparent {
        let Some(canonical) = mapping.resolve(name) else {
            let visible: Vec<String> = mapping.entries.iter().map(|(apparent, _)| format!("@{apparent}")).collect();
            let suggestion = bazel_starlark::reported::did_you_mean(name, &visible);
            return Err(SkyError::new(format!(
                "no repository visible as '@{name}'{suggestion} from the main repository, but overridden with --override_repository. Use --inject_repository to add new repositories."
            )));
        };
        if canonical == repo {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// How a repository is made: from the dependency graph for modules and
/// `use_repo_rule` repositories, or from a module extension's result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoSpecKey(pub RepoName);

impl Key for RepoSpecKey {
    type Value = RepoSpec;
    const KIND: &'static str = "REPOSITORY_SPEC";

    async fn compute(&self, ctx: &Ctx) -> Result<RepoSpec, SkyError> {
        let repo = self.0;
        let command = ctx.compute(&CommandOverridesKey).await?;
        if let Some(path) = command.repository(repo.as_str()) {
            return Ok(RepoSpec::Local { path: path.to_string() });
        }
        if repo.as_str() == "local_config_platform" {
            return Ok(RepoSpec::HostPlatform);
        }
        let graph = ctx.compute(&DepGraphKey).await?;
        if let Some(path) = overridden_by_apparent_name(&graph, &command, repo)? {
            return Ok(RepoSpec::Local { path: path.to_string() });
        }
        if let Some((_, module)) = graph.module(repo) {
            return module.spec.clone().ok_or_else(|| SkyError::new("the main repository isn't fetched"));
        }
        if let Some((module_index, call_index)) = graph.repo_rule_of(repo) {
            let mapping = graph.module_mapping(module_index);
            let module = &graph.modules[module_index];
            let call = &module.file.repo_rules[call_index];
            let context = LabelContext {
                package: PackageId::new(module.repo, PathId::ROOT),
                mapping: &mapping,
                lenient: false,
            };
            let bzl = Label::parse(&call.bzl, &context).map_err(|error| SkyError::new(error.to_string()))?;
            return Ok(RepoSpec::RepoRule(RepoRuleSpec {
                bzl,
                rule: call.rule.clone(),
                attrs: call.attrs.clone(),
                base_repo: module.repo,
                original_name: call.name.clone(),
            }));
        }
        if let Some((extension, name)) = graph.extension_of(repo) {
            let result = ctx.compute(&crate::extension::ExtensionKey(extension as u32)).await?;
            return result
                .repos
                .iter()
                .find(|(repo_name, _)| repo_name == name)
                .map(|(_, spec)| RepoSpec::RepoRule(spec.clone()))
                .ok_or_else(|| SkyError::new(format!("module extension {} doesn't generate a repository named '{name}'", graph.extensions[extension].name)));
        }
        Err(SkyError::new(format!("no repository is visible as '@@{repo}'")))
    }
}
