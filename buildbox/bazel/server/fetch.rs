//! `bb fetch`: fetch the external repositories some targets need, those
//! `--repo` names, or with `--all` or nothing named, every one the module
//! graph defines.

use crate::build;
use crate::command::{CommandContext, exit};
use crate::output::Output;
use bazel_bzlmod::extension::ExtensionKey;
use bazel_bzlmod::fetch_repo::RepoFetchKey;
use bazel_bzlmod::resolve::{DepGraphKey, repo_rule_repos};
use bazel_label::{RepoMapping, RepoMappingTable, RepoName};
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use skyframe::Session;
use std::time::Instant;

pub async fn fetch(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let started = Instant::now();
    let mut spec = build::build_spec().options();
    spec.extend([("all", OptionKind::Bool), ("repo", OptionKind::Multi)]);
    let mut options = match crate::queries::parse_options(context, "fetch", args, &OptionSpec::new(&spec), out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let repos: Vec<String> = options.all("repo").into_iter().map(str::to_string).collect();
    let kinds = [!options.residue.is_empty(), !repos.is_empty(), options.flag("all", false)];
    if kinds.iter().filter(|given| **given).count() > 1 {
        out.error("Only one fetch option can be provided for fetch command").await;
        return exit::COMMAND_LINE_ERROR;
    }
    if !options.residue.is_empty() {
        options.set("build", "false");
        return fetch_targets(context, &options, started, out).await;
    }
    let workspace = context.server.workspaces().get(&context.workspace_root);
    crate::command::refresh(context, &workspace, &options).await;
    let session = workspace.graph.session().await;
    let main_mapping = match crate::command::main_mapping(context, &session, &options, out).await {
        Ok(mapping) => mapping,
        Err(code) => return code,
    };
    let (wanted, done) = match repos.is_empty() {
        true => (all_repos(&session).await, "All external dependencies fetched successfully."),
        false => (named_repos(&repos, &main_mapping), "All requested repos fetched successfully."),
    };
    let wanted = match wanted {
        Ok(wanted) => wanted,
        Err(message) => {
            out.error(&message).await;
            return exit::INTERRUPTED;
        }
    };
    let keys: Vec<RepoFetchKey> = wanted.into_iter().map(RepoFetchKey).collect();
    let mut failed = false;
    for result in session.evaluate_many(&keys).await {
        if let Err(error) = result {
            out.error(error.message()).await;
            failed = true;
        }
    }
    if failed {
        return exit::EXTERNAL_DEPS_ERROR;
    }
    out.info(done).await;
    exit::SUCCESS
}

/// Analyze the targets, which fetches the repositories they need.
async fn fetch_targets(context: &CommandContext<'_>, options: &ParsedOptions, started: Instant, out: &Output) -> i32 {
    let workspace = context.server.workspaces().get(&context.workspace_root);
    crate::command::refresh(context, &workspace, options).await;
    let session = workspace.graph.session().await;
    let built = match build::run(context, &session, options, out).await {
        Ok(built) => built,
        Err(code) => return build::failed(code, started, out).await,
    };
    build::report_stats(started, out).await;
    if built.errors {
        out.error("Build did NOT complete successfully").await;
        return exit::BUILD_FAILURE;
    }
    build::completed(0, out).await;
    out.info("All external dependencies for the requested targets fetched successfully.").await;
    exit::SUCCESS
}

/// The repositories `--repo` names: `@apparent`, as the main repository
/// sees it, or `@@canonical`.
fn named_repos(names: &[String], main_mapping: &RepoMappingTable) -> Result<Vec<RepoName>, String> {
    let mut repos = Vec::with_capacity(names.len());
    for name in names {
        let repo = match (name.strip_prefix("@@"), name.strip_prefix('@')) {
            (Some(canonical), _) => RepoName::new(canonical),
            (None, Some(apparent)) => match main_mapping.resolve(apparent) {
                Some(repo) => repo,
                None => return Err(format!("Fetching some repos failed with errors: No repository visible as '@{apparent}' from main repository")),
            },
            (None, None) => return Err("Invalid repo name: The repo value has to be either apparent '@repo' or canonical '@@repo' repo name".to_string()),
        };
        repos.push(repo);
    }
    Ok(repos)
}

/// Every repository the module graph defines: the modules', those of
/// `use_repo_rule`, and those module extensions generate.
async fn all_repos(session: &Session) -> Result<Vec<RepoName>, String> {
    let graph = session.evaluate(&DepGraphKey).await.map_err(|error| error.message().to_string())?;
    let mut repos: Vec<RepoName> = graph.modules.iter().map(|module| module.repo).filter(|repo| !repo.is_main()).collect();
    for module in &graph.modules {
        repos.extend(repo_rule_repos(module));
    }
    let keys: Vec<ExtensionKey> = (0..graph.extensions.len() as u32).map(ExtensionKey).collect();
    for (extension, result) in graph.extensions.iter().zip(session.evaluate_many(&keys).await) {
        let result = result.map_err(|error| error.message().to_string())?;
        repos.extend(result.repos.iter().map(|(name, _)| RepoName::new(&format!("{}+{name}", extension.prefix))));
    }
    Ok(repos)
}
