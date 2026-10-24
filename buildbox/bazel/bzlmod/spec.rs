//! Finding the spec of any repository.

use crate::repo_spec::{RepoRuleSpec, RepoSpec};
use crate::resolve::DepGraphKey;
use bazel_label::{Label, LabelContext, PackageId, RepoName};
use intern::PathId;
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError};

/// How a repository is made: from the dependency graph for modules and
/// `use_repo_rule` repositories, or from a module extension's result.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoSpecKey(pub RepoName);

impl Key for RepoSpecKey {
    type Value = RepoSpec;
    const KIND: &'static str = "REPOSITORY_SPEC";

    async fn compute(&self, ctx: &Ctx) -> Result<RepoSpec, SkyError> {
        let repo = self.0;
        if repo.as_str() == "local_config_platform" {
            return Ok(RepoSpec::HostPlatform);
        }
        let graph = ctx.compute(&DepGraphKey).await?;
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
