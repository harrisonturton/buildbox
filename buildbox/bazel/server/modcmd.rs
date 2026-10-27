//! `bb mod`: inspect the module graph. Of Bazel's subcommands, bb has
//! `dump_repo_mapping`.

use crate::build;
use crate::command::{CommandContext, exit};
use crate::output::Output;
use bazel_bzlmod::resolve::DepGraphKey;
use bazel_label::RepoName;
use bazel_options::OptionSpec;

/// Bazel's `mod` subcommands.
const SUBCOMMANDS: &str = "deps, graph, all_paths, path, explain, show_repo, show_extension, dump_repo_mapping, tidy";

pub async fn mod_command(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let spec = OptionSpec::new(&build::build_spec().options());
    let options = match crate::queries::parse_options(context, "mod", args, &spec, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let Some((subcommand, arguments)) = options.residue.split_first() else {
        out.error(&format!("No subcommand specified, choose one of : ({SUBCOMMANDS}).")).await;
        return exit::INTERNAL_ERROR;
    };
    if subcommand != "dump_repo_mapping" {
        if !SUBCOMMANDS.split(", ").any(|known| known == subcommand) {
            out.error(&format!("Invalid subcommand, choose one from : ({SUBCOMMANDS}).")).await;
            return exit::COMMAND_LINE_ERROR;
        }
        out.error(&format!("'mod {subcommand}' isn't supported by bb yet")).await;
        return exit::COMMAND_LINE_ERROR;
    }
    if arguments.is_empty() {
        out.error("No repository name(s) specified. Type 'bb help mod' for syntax and help.").await;
        return exit::COMMAND_LINE_ERROR;
    }
    let workspace = context.server.workspaces().get(&context.workspace_root);
    crate::command::refresh(context, &workspace, &options).await;
    let session = workspace.graph.session().await;
    if let Err(code) = crate::command::main_mapping(context, &session, &options, out).await {
        return code;
    }
    let graph = match session.evaluate(&DepGraphKey).await {
        Ok(graph) => graph,
        Err(error) => {
            out.error(error.message()).await;
            return exit::EXTERNAL_DEPS_ERROR;
        }
    };
    let mut mappings = Vec::with_capacity(arguments.len());
    let mut missing = Vec::new();
    for name in arguments {
        match graph.module(RepoName::new(name)) {
            Some((index, _)) => mappings.push(graph.ordered_module_mapping(index)),
            None => missing.push(name.as_str()),
        }
    }
    if !missing.is_empty() {
        out.error(&format!("Repositories not found: {}. Type 'bb help mod' for syntax and help.", missing.join(", "))).await;
        return exit::COMMAND_LINE_ERROR;
    }
    let text: String = mappings.iter().map(|mapping| format!("{}\n", json_object(mapping))).collect();
    out.stdout(&text).await;
    exit::SUCCESS
}

/// A mapping as one line of JSON, as Bazel prints it.
fn json_object(mapping: &[(String, RepoName)]) -> String {
    let entries: Vec<String> = mapping.iter().map(|(apparent, repo)| format!("{}:{}", json_string(apparent), json_string(repo.as_str()))).collect();
    format!("{{{}}}", entries.join(","))
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}
