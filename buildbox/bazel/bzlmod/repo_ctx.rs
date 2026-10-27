//! `repository_ctx`, and running repository rules.

use crate::attrs::{AttrContext, apply_schema};
use crate::config::BzlmodConfig;
use crate::fetch_repo::{FetchedRepo, RepoFetchKey, fetch_into, repo_dir};
use crate::graph_access::GraphAccess;
use crate::io::{HasIo, IoState};
use crate::path_value::PathValue;
use crate::repo_env::ClientEnv;
use crate::repo_spec::RepoRuleSpec;
use crate::star_value::StarValue;
use allocative::Allocative;
use bazel_fs::{FileContentKey, Root, RootedPath};
use bazel_label::{RepoMappingTable, RepoName};
use bazel_loading::{BzlLoadKey, BzlModule, RepoMappingKey};
use bazel_starlark::repo_api::RepositoryRule;
use bazel_starlark::{EvalContext, FileKind};
use intern::{PathId, Symbol};
use skycodec::Fingerprinter;
use skyframe::{Ctx, SkyError};
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, starlark_value};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// `repository_ctx`: what a repository rule's implementation is given.
#[derive(ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct RepositoryCtx {
    #[allocative(skip)]
    io: IoState,
    name: String,
    original_name: String,
    #[allocative(skip)]
    attrs: Vec<(String, StarValue)>,
}

starlark_simple_value!(RepositoryCtx);

impl fmt::Debug for RepositoryCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "repository_ctx({})", self.name)
    }
}

impl fmt::Display for RepositoryCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository_ctx for {}>", self.name)
    }
}

impl HasIo for RepositoryCtx {
    fn io(&self) -> &IoState {
        &self.io
    }
}

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for RepositoryCtx {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("repository_ctx", |builder| {
            crate::io::io_methods(builder);
            repo_methods(builder);
        });
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => {
                let mut fields: Vec<(&str, Value<'v>)> = vec![("name", heap.alloc(self.name.as_str()))];
                fields.extend(self.attrs.iter().map(|(name, value)| (name.as_str(), value.to_value(heap))));
                Some(heap.alloc(AllocStruct(fields)))
            }
            "name" => Some(heap.alloc(self.name.as_str())),
            "original_name" => Some(heap.alloc(self.original_name.as_str())),
            "workspace_root" => self.io.graph.workspace_root().ok().map(|root| heap.alloc(PathValue(root))),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["attr", "name", "original_name", "workspace_root"].iter().map(|s| s.to_string()).collect()
    }
}

#[starlark_module]
fn repo_methods(builder: &mut MethodsBuilder) {
    /// Bazel 8: whether the repository can be cached. Accepted, and the
    /// repository is cached either way.
    fn repo_metadata<'v>(
        this: Value<'v>,
        #[starlark(require = named, default = false)] reproducible: bool,
        #[starlark(require = named)] attrs_for_reproducibility: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (this, reproducible, attrs_for_reproducibility);
        Ok(NoneType)
    }
}

/// Fetch a repository by running its repository rule.
pub async fn fetch(ctx: &Ctx, repo: RepoName, spec: &RepoRuleSpec) -> Result<FetchedRepo, SkyError> {
    let config = ctx.global::<BzlmodConfig>()?;
    let module = ctx.compute(&BzlLoadKey(spec.bzl)).await?;
    let mappings = mappings(ctx, &module, spec.base_repo).await?;
    let inputs = main_repo_inputs(ctx, spec).await?;
    let definer = defining_repo_dir(ctx, spec).await?;
    let env_inputs = crate::repo_env::declared(ctx, &rule_environ(&module, spec)).await?;
    let environment = ctx.global::<ClientEnv>()?.current();
    let stable_spec = format!("{spec:?}");
    let local = local_inputs(&module, spec);
    let dest = repo_dir(&config, repo, &[stable_spec.as_bytes(), definer.as_bytes(), &inputs, env_inputs.as_bytes(), local.as_bytes()].concat());
    let access = GraphAccess::new(ctx.clone());
    let downloads = config.downloads_for(ctx)?;
    let spec = spec.clone();
    let span = tracing::info_span!("bzlmod.repository_rule", repo = %repo, rule = %spec.rule);
    let fetched_dest = dest.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        fetch_into(&fetched_dest, |dir| {
            let io = IoState {
                base_dir: dir.to_path_buf(),
                graph: access,
                downloads,
                env: environment.env.iter().cloned().collect(),
            };
            run_rule(&module, &spec, repo, io, mappings).map_err(|error| format!("fetching @@{repo}: {error}"))
        })
    })
    .await;
    result.map_err(|error| SkyError::transient(error.to_string()))??;
    Ok(FetchedRepo {
        dir: Symbol::intern(&dest.display().to_string()),
    })
}

/// The mappings a rule's code needs: its own repository's, the
/// repositories its module loads from, and the one its attributes are
/// relative to.
async fn mappings(ctx: &Ctx, module: &BzlModule, base: RepoName) -> Result<HashMap<RepoName, Arc<RepoMappingTable>>, SkyError> {
    let mut repos = module.repos.clone();
    repos.push(base);
    repos.sort();
    repos.dedup();
    let keys: Vec<RepoMappingKey> = repos.iter().map(|repo| RepoMappingKey(*repo)).collect();
    let mut mappings = HashMap::new();
    for (repo, mapping) in repos.into_iter().zip(ctx.compute_many(&keys).await) {
        mappings.insert(repo, Arc::new(mapping?.0.clone()));
    }
    Ok(mappings)
}

/// The variables a repository is fetched with that identify it: the
/// `--repo_env` ones, and those its rule declares it reads. The rest of the
/// client's environment, such as `PATH`, varies between clients that should
/// share repositories.
/// The variables a repository rule declares, with `environ`, that it reads.
/// What a `local` rule's repository depends on besides its spec: the Bazel
/// version bb behaves like, which such rules read, as rules_cc's does to
/// choose between Bazel's C++ rules and its own. Bazel runs local rules
/// again when its server restarts; bb, when what they read changes.
fn local_inputs(module: &BzlModule, spec: &RepoRuleSpec) -> String {
    let Ok((value, _)) = module.module.get_any_visibility(&spec.rule) else {
        return String::new();
    };
    match RepositoryRule::from_value(value.value()).is_some_and(|rule| rule.local) {
        true => format!("local, Bazel {}", bazel_starlark::BAZEL_VERSION),
        false => String::new(),
    }
}

fn rule_environ(module: &BzlModule, spec: &RepoRuleSpec) -> Vec<String> {
    let Ok((value, _)) = module.module.get_any_visibility(&spec.rule) else {
        return Vec::new();
    };
    RepositoryRule::from_value(value.value()).map(|rule| rule.environ.clone()).unwrap_or_default()
}

/// The directory of the repository that defines the rule, which names the
/// version of its module: a rule's label is the same in every version.
async fn defining_repo_dir(ctx: &Ctx, spec: &RepoRuleSpec) -> Result<String, SkyError> {
    let repo = spec.bzl.repo();
    if repo.is_main() {
        return Ok(String::new());
    }
    let fetched = ctx.compute(&RepoFetchKey(repo)).await?;
    Ok(fetched.dir.as_str().to_string())
}

/// The contents of the main repository's files that the rule's attributes
/// name, such as a `build_file`. Repositories are shared between
/// workspaces, so these must be part of what identifies the repository.
async fn main_repo_inputs(ctx: &Ctx, spec: &RepoRuleSpec) -> Result<Vec<u8>, SkyError> {
    let mut labels = Vec::new();
    collect_main_labels(&spec.attrs.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>(), spec.base_repo, &mut labels);
    let mut fingerprinter = Fingerprinter::new();
    for path in labels {
        let content = ctx.compute(&FileContentKey(RootedPath::new(Root::Workspace, path))).await?;
        fingerprinter.bytes(content.digest.as_ref().map(|digest| &digest.as_bytes()[..]).unwrap_or(b""));
    }
    Ok(fingerprinter.finish().as_bytes().to_vec())
}

fn collect_main_labels(values: &[StarValue], base: RepoName, out: &mut Vec<PathId>) {
    for value in values {
        match value {
            StarValue::Label(label) if label.repo().is_main() => {
                if let Ok(name) = PathId::parse(label.name.as_str()) {
                    out.push(label.package.path.join_path(name));
                }
            }
            StarValue::String(text) if base.is_main() && (text.starts_with("//") || text.starts_with("@//")) => {
                if let Ok(label) = bazel_label::Label::parse_canonical(text.trim_start_matches('@')) {
                    if let Ok(name) = PathId::parse(label.name.as_str()) {
                        out.push(label.package.path.join_path(name));
                    }
                }
            }
            StarValue::List(items) => collect_main_labels(items, base, out),
            StarValue::Dict(entries) => {
                for (k, v) in entries {
                    collect_main_labels(std::slice::from_ref(k), base, out);
                    collect_main_labels(std::slice::from_ref(v), base, out);
                }
            }
            _ => {}
        }
    }
}

/// Run a repository rule's implementation in `io.base_dir`.
fn run_rule(
    module: &BzlModule,
    spec: &RepoRuleSpec,
    repo: RepoName,
    io: IoState,
    mappings: HashMap<RepoName, Arc<RepoMappingTable>>,
) -> anyhow::Result<()> {
    let empty = Arc::new(RepoMappingTable::new(spec.base_repo, Vec::new()));
    let base_mapping = mappings.get(&spec.base_repo).cloned().unwrap_or_else(|| empty.clone());
    let rule_mapping = mappings.get(&spec.bzl.repo()).cloned().unwrap_or(empty);
    let given = AttrContext::repo_root(spec.base_repo, &*base_mapping);
    // Defaults are relative to the file defining the rule.
    let defaults = AttrContext {
        package: spec.bzl.package,
        mapping: &*rule_mapping,
    };
    let what = format!("{}%{}", spec.bzl, spec.rule);
    let mut context = EvalContext::new(FileKind::Bzl, spec.bzl);
    context.mappings = mappings.clone();
    let graph = io.graph.clone();
    let result = Module::with_temp_heap(|env| -> anyhow::Result<()> {
        let (value, _) = module
            .module
            .get_any_visibility(&spec.rule)
            .map_err(|_| anyhow::anyhow!("{} doesn't export '{}'", spec.bzl, spec.rule))?;
        let value = env.heap().access_owned_frozen_value(&value);
        let Some(rule) = RepositoryRule::from_value(value) else {
            anyhow::bail!("{what} isn't a repository rule");
        };
        let attrs = apply_schema(&what, &rule.attrs, &spec.attrs, &given, &defaults)?;
        let ctx = RepositoryCtx {
            io,
            name: repo.to_string(),
            original_name: spec.original_name.clone(),
            attrs,
        };
        let mut eval = Evaluator::new(&env);
        eval.extra = Some(&context);
        let ctx_value = env.heap().alloc(ctx);
        eval.eval_function(rule.implementation, &[ctx_value], &[]).map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(())
    });
    graph.report_printed(&context.printed.take());
    result
}
