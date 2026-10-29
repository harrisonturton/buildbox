//! `bb mod`: inspect the module graph. Of Bazel's subcommands, bb has
//! `graph`, `deps` and `dump_repo_mapping`.

use crate::build;
use crate::command::{CommandContext, exit};
use crate::output::Output;
use crate::mod_graph::{GraphModule, GraphOptions};
use bazel_bzlmod::repo_spec::RepoSpec;
use bazel_bzlmod::resolve::{DepGraph, DepGraphKey};
use bazel_options::{OptionKind, ParsedOptions};
use bazel_label::RepoName;
use bazel_options::OptionSpec;

/// Bazel's `mod` subcommands.
const SUBCOMMANDS: &str = "deps, graph, all_paths, path, explain, show_repo, show_extension, dump_repo_mapping, tidy";

/// The options `mod` takes besides `build`'s.
const MOD_OPTIONS: &[(&str, OptionKind)] = &[
    ("depth", OptionKind::Value),
    ("charset", OptionKind::Value),
    ("include_builtin", OptionKind::Bool),
    ("cycles", OptionKind::Bool),
    ("output", OptionKind::Value),
];

pub async fn mod_command(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let mut options = build::build_spec().options();
    options.extend_from_slice(MOD_OPTIONS);
    let spec = OptionSpec::new(&options);
    let options = match crate::queries::parse_options(context, "mod", args, &spec, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let Some((subcommand, arguments)) = options.residue.split_first() else {
        out.error(&format!("No subcommand specified, choose one of : ({SUBCOMMANDS}).")).await;
        return exit::INTERNAL_ERROR;
    };
    if !SUBCOMMANDS.split(", ").any(|known| known == subcommand) {
        out.error(&format!("Invalid subcommand, choose one from : ({SUBCOMMANDS}).")).await;
        return exit::COMMAND_LINE_ERROR;
    }
    if !matches!(subcommand.as_str(), "graph" | "deps" | "dump_repo_mapping") {
        out.error(&format!("'mod {subcommand}' isn't supported by bb yet")).await;
        return exit::COMMAND_LINE_ERROR;
    }
    if subcommand == "dump_repo_mapping" && arguments.is_empty() {
        out.error("No repository name(s) specified. Type 'bb help mod' for syntax and help.").await;
        return exit::COMMAND_LINE_ERROR;
    }
    let workspace = context.server.workspaces().get(&context.workspace_root);
    if let Err(message) = crate::command::refresh(context, &workspace, &options).await {
        out.error(&message).await;
        return crate::command::exit::COMMAND_LINE_ERROR;
    }
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
    if subcommand != "dump_repo_mapping" {
        return graph_command(&graph, subcommand, arguments, &options, out).await;
    }
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

/// `mod graph` or `mod deps`: the tree of modules from the root, or from
/// the modules the arguments name.
async fn graph_command(graph: &DepGraph, subcommand: &str, arguments: &[String], options: &ParsedOptions, out: &Output) -> i32 {
    let modules = graph_modules(graph);
    let graph_options = match graph_options(subcommand, options) {
        Ok(graph_options) => graph_options,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let from = match (subcommand, arguments) {
        ("deps", []) => {
            out.error("No module(s) specified. Type 'bb help mod' for syntax and help.").await;
            return exit::COMMAND_LINE_ERROR;
        }
        (_, []) => vec![0],
        (_, arguments) => match module_args(graph, &modules, arguments) {
            Ok(from) => from,
            Err(message) => {
                out.error(&message).await;
                return exit::COMMAND_LINE_ERROR;
            }
        },
    };
    out.stdout(&crate::mod_graph::draw(&modules, &from, &graph_options)).await;
    exit::SUCCESS
}

/// The graph's modules as `mod graph` draws them, the root first.
fn graph_modules(graph: &DepGraph) -> Vec<GraphModule> {
    let mut order: Vec<usize> = (0..graph.modules.len()).collect();
    order.sort_by_key(|index| !graph.modules[*index].repo.is_main());
    let position = |repo| order.iter().position(|index| graph.modules[*index].repo == repo);
    order
        .iter()
        .map(|index| {
            let module = &graph.modules[*index];
            let from_registry = !matches!(module.spec, Some(RepoSpec::Local { .. } | RepoSpec::Override(_) | RepoSpec::Builtin { .. }));
            GraphModule {
                name: module.name.clone(),
                version: if module.repo.is_main() || from_registry { module.version.clone() } else { String::new() },
                deps: module.deps.iter().filter_map(|(_, repo)| position(*repo)).collect(),
                builtin: matches!(module.spec, Some(RepoSpec::Builtin { .. })),
            }
        })
        .collect()
}

/// How the options say to draw the graph. Those of Bazel's that bb can't
/// do yet are errors.
fn graph_options(subcommand: &str, options: &ParsedOptions) -> Result<GraphOptions, String> {
    if let Some(output) = options.get("output").filter(|output| *output != "text") {
        return Err(format!("--output={output} :: bb doesn't support --output={output} for mod yet"));
    }
    let depth = match options.get("depth") {
        Some(depth) => depth.parse::<i64>().map_err(|_| format!("While parsing option --depth={depth}: '{depth}' is not an int"))?,
        None => -1,
    };
    let depth = match (depth, subcommand) {
        (depth, _) if depth >= 1 => depth as usize,
        (_, "deps") => 2,
        _ => usize::MAX,
    };
    let ascii = match options.get("charset").unwrap_or("utf8") {
        "utf8" => false,
        "ascii" => true,
        other => return Err(format!("While parsing option --charset={other}: Not a valid charset: '{other}' (should be ascii or utf8)")),
    };
    Ok(GraphOptions {
        depth,
        include_builtin: options.flag("include_builtin", false),
        cycles: options.flag("cycles", true),
        ascii,
    })
}

/// The modules the arguments name, as Bazel's module arguments do:
/// `<root>`, a name, which is every version of it, `name@version`, or
/// `@repo`, a repository the root module sees.
fn module_args(graph: &DepGraph, modules: &[GraphModule], arguments: &[String]) -> Result<Vec<usize>, String> {
    let mut from = Vec::new();
    for argument in arguments {
        let found: Vec<usize> = match argument.as_str() {
            "<root>" => vec![0],
            _ if argument.starts_with('@') => repo_module(graph, modules, &argument[1..]).into_iter().collect(),
            _ => match argument.split_once('@') {
                Some((name, version)) => {
                    let version = if version == "_" { "" } else { version };
                    modules.iter().enumerate().filter(|(_, module)| module.name == name && module.version == version).map(|(index, _)| index).collect()
                }
                None => modules.iter().enumerate().filter(|(_, module)| module.name == *argument).map(|(index, _)| index).collect(),
            },
        };
        if found.is_empty() {
            return Err(format!("In argument #1 '{argument}': Module {argument} does not exist in the dependency graph. Type 'bb help mod' for syntax and help."));
        }
        from.extend(found);
    }
    Ok(from)
}

/// The module whose repository the root module sees as `apparent`.
fn repo_module(graph: &DepGraph, modules: &[GraphModule], apparent: &str) -> Option<usize> {
    let root = graph.modules.iter().position(|module| module.repo.is_main())?;
    let repo = graph.modules[root].deps.iter().find(|(name, _)| name == apparent).map(|(_, repo)| *repo)?;
    let module = graph.modules.iter().find(|module| module.repo == repo)?;
    modules.iter().position(|candidate| candidate.name == module.name)
}

/// A mapping as one line of JSON, as Bazel prints it.
fn json_object(mapping: &[(String, RepoName)]) -> String {
    let entries: Vec<String> = mapping.iter().map(|(apparent, repo)| format!("{}:{}", json_string(apparent), json_string(repo.as_str()))).collect();
    format!("{{{}}}", entries.join(","))
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_string()).to_string()
}
