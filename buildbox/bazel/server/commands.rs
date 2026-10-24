//! The commands bb implements.

use crate::command::{BAZEL_VERSION, CommandContext, exit, working_package};
use crate::output::Output;
use bazel_label::RepoName;
use bazel_loading::RepoMappingKey;
use bazel_options::{OptionKind, OptionSpec};
use bazel_query::{OutputFormat, QueryOptions, QueryRequest};
use std::path::PathBuf;
use std::sync::Arc;

pub async fn version(args: &[String], out: &Output) -> i32 {
    if args.iter().any(|arg| arg == "--gnu_format") {
        out.stdout(&format!("bazel {BAZEL_VERSION}\n")).await;
        return exit::SUCCESS;
    }
    out.stdout(&format!(
        "Build label: {BAZEL_VERSION}\nBuild target: bb {}\nBuild time: unknown\nBuild timestamp: 0\nBuild timestamp as int: 0\n",
        env!("CARGO_PKG_VERSION")
    ))
    .await;
    exit::SUCCESS
}

pub(crate) fn query_spec() -> OptionSpec {
    use OptionKind::*;
    OptionSpec::new(&[
        ("output", Value),
        ("keep_going", Bool),
        ("implicit_deps", Bool),
        ("tool_deps", Bool),
        ("host_deps", Bool),
        ("order_output", Value),
        ("order_results", Bool),
        ("universe_scope", Value),
        ("infer_universe_scope", Bool),
        ("query_file", Value),
        ("null", Bool),
        ("line_terminator_null", Bool),
        ("relative_locations", Bool),
        ("output_file", Value),
        ("noproto:default_values", Bool),
        ("xml:default_values", Bool),
        ("graph:factored", Bool),
        ("strict_test_suite", Bool),
        ("loading_phase_threads", Value),
        ("experimental_graphless_query", Value),
        ("repo_env", Multi),
    ])
}

pub async fn query(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let all_args = bazel_options::command_args(&context.rc, "query", args);
    let options = match bazel_options::parse(&all_args, &query_spec(), &context.rc, "query") {
        Ok(options) => options,
        Err(error) => {
            out.error(&error.to_string()).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let expression = match query_expression(&options) {
        Ok(expression) => expression,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let output_name = options.get("output").unwrap_or("label");
    let Some(output) = OutputFormat::parse(output_name) else {
        out.error(&format!("Invalid output format '{output_name}'. Valid values are: label, label_kind, build, minrank, maxrank, package, location, graph")).await;
        return exit::COMMAND_LINE_ERROR;
    };
    if let Err(error) = bazel_query::parse(&expression) {
        out.error(&format!("Error while parsing '{expression}': {}", error.0)).await;
        return exit::COMMAND_LINE_ERROR;
    }

    let query_options = QueryOptions {
        implicit_deps: options.flag("implicit_deps", true),
        tool_deps: options.flag("tool_deps", true) && options.flag("host_deps", true),
        keep_going: options.flag("keep_going", false),
    };
    let workspace = context.server.workspaces().get(&context.workspace_root);
    workspace.refresh(crate::command::repo_environment(context, &options)).await;
    let session = workspace.graph.session().await;
    let main_mapping = match session.evaluate(&RepoMappingKey(RepoName::MAIN)).await {
        Ok(mapping) => Arc::new(mapping.0.clone()),
        Err(error) => {
            out.error(error.message()).await;
            return exit::ANALYSIS_FAILURE;
        }
    };
    let request = QueryRequest {
        expression,
        output,
        options: query_options,
        working_package: working_package(&context.workspace_root, &context.working_directory),
        main_mapping,
        workspace_root: context.workspace_root.clone(),
    };
    let result = bazel_query::run(session, request).await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            out.error(&error.0).await;
            return exit::ANALYSIS_FAILURE;
        }
    };
    for error in &result.errors {
        out.error(&format!("Skipping: {error}")).await;
    }
    let output = if options.flag("null", false) || options.flag("line_terminator_null", false) {
        result.output.replace('\n', "\0")
    } else {
        result.output
    };
    out.stdout(&output).await;
    if !result.errors.is_empty() {
        out.warning("--keep_going specified, ignoring errors. Results may be inaccurate").await;
        return exit::PARTIAL_ANALYSIS_FAILURE;
    }
    if result.targets == 0 {
        out.info("Empty results").await;
    }
    exit::SUCCESS
}

fn query_expression(options: &bazel_options::ParsedOptions) -> Result<String, String> {
    if let Some(file) = options.get("query_file") {
        if !options.residue.is_empty() {
            return Err("Command-line query and --query_file cannot both be specified".into());
        }
        return std::fs::read_to_string(file).map_err(|error| format!("failed to read --query_file {file}: {error}"));
    }
    if options.residue.is_empty() {
        return Err("missing query expression. Type 'bazel help query' for syntax and help.".into());
    }
    Ok(options.residue.join(" "))
}

/// The directory a workspace's outputs go in: like Bazel's output base, one
/// per workspace, named by a hash of the workspace's path.
pub fn output_base(context: &CommandContext<'_>) -> PathBuf {
    output_base_for(context.server, &context.workspace_root)
}

/// The output base of the workspace at `root`.
pub fn output_base_for(server: &crate::BazelServer, root: &std::path::Path) -> PathBuf {
    let digest = skycodec::Fingerprint::of(root.display().to_string().as_bytes());
    server.config().bazel_dir.join("output_bases").join(digest.to_string())
}

/// The keys `info` prints, with Bazel's descriptions of them.
pub const INFO_KEYS: &[(&str, &str)] = &[
    ("bazel-bin", "Configuration dependent directory for binaries."),
    ("bazel-genfiles", "Configuration dependent directory for generated files."),
    ("bazel-testlogs", "Configuration dependent directory for logs from a test run."),
    ("execution_root", "A directory that makes all input and output files visible to the build."),
    ("install_base", "The installation base directory."),
    ("output_base", "A directory for shared bazel state as well as tool and strategy specific subdirectories."),
    ("output_path", "Output directory"),
    ("package_path", "The search path for resolving package labels."),
    ("release", "bazel release identifier"),
    ("repository_cache", "The location of the repository download cache used"),
    ("server_pid", "bazel process id"),
    (
        "used-heap-size",
        "The amount of used memory in bytes. Note that this is not a good indicator of the actual memory use, as it includes any remaining inaccessible memory.",
    ),
    ("workspace", "The working directory of the server."),
];

pub async fn info(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let entries = info_values(context);
    let keys: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if keys.is_empty() {
        let text: String = entries.iter().map(|(key, value)| format!("{key}: {value}\n")).collect();
        out.stdout(&text).await;
        return exit::SUCCESS;
    }
    let mut text = String::new();
    for key in &keys {
        let Some((_, value)) = entries.iter().find(|(name, _)| name == key) else {
            out.error(&format!("unknown key: '{key}'")).await;
            return exit::COMMAND_LINE_ERROR;
        };
        if keys.len() == 1 {
            text.push_str(&format!("{value}\n"));
        } else {
            text.push_str(&format!("{key}: {value}\n"));
        }
    }
    out.stdout(&text).await;
    exit::SUCCESS
}

/// The value of each of [`INFO_KEYS`], in order.
fn info_values(context: &CommandContext<'_>) -> Vec<(&'static str, String)> {
    let output_base = output_base(context);
    let execution_root = output_base.join("execroot/_main");
    let output_path = execution_root.join("bazel-out");
    let bin = output_path.join("k8-fastbuild/bin");
    let bazel_dir = &context.server.config().bazel_dir;
    let engine = context.server.workspaces().engine().stats();
    let value = |key: &str| match key {
        "bazel-bin" | "bazel-genfiles" => bin.display().to_string(),
        "bazel-testlogs" => output_path.join("k8-fastbuild/testlogs").display().to_string(),
        "execution_root" => execution_root.display().to_string(),
        "install_base" => bazel_dir.join("install").display().to_string(),
        "output_base" => output_base.display().to_string(),
        "output_path" => output_path.display().to_string(),
        "package_path" => "%workspace%".to_string(),
        "release" => format!("release {BAZEL_VERSION}"),
        "repository_cache" => bazel_dir.join("repository_cache").display().to_string(),
        "server_pid" => std::process::id().to_string(),
        "used-heap-size" => format!("{}MB", (engine.store.stored_bytes + engine.store.entry_bytes) >> 20),
        _ => context.workspace_root.display().to_string(),
    };
    INFO_KEYS.iter().map(|(key, _)| (*key, value(key))).collect()
}

pub async fn shutdown(context: &CommandContext<'_>, out: &Output) -> i32 {
    if context.server.workspaces().remove(&context.workspace_root) {
        out.info("Freed this workspace's build graph.").await;
    }
    exit::SUCCESS
}

pub async fn clean(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let output_base = output_base(context);
    let expunge = args.iter().any(|arg| arg == "--expunge" || arg == "--expunge_async");
    let target = if expunge { output_base.clone() } else { output_base.join("execroot") };
    let removed = tokio::task::spawn_blocking(move || match std::fs::remove_dir_all(&target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to remove {}: {error}", target.display())),
    })
    .await;
    match removed {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            out.error(&message).await;
            return exit::BUILD_FAILURE;
        }
        Err(error) => {
            out.error(&error.to_string()).await;
            return exit::INTERNAL_ERROR;
        }
    }
    if expunge {
        context.server.workspaces().remove(&context.workspace_root);
    }
    out.info("Starting clean.").await;
    exit::SUCCESS
}

/// `page-check`: page every analyzed target in the workspace out of
/// memory and read each back, reporting how long it took and what failed.
/// A diagnostic.
pub async fn page_check(context: &CommandContext<'_>, out: &Output) -> i32 {
    let workspace = context.server.workspaces().get(&context.workspace_root);
    let keys = workspace.graph.keys::<bazel_analysis::ConfiguredTargetKey>();
    let session = workspace.graph.session().await;
    let targets: Vec<_> = session.evaluate_many(&keys).await.into_iter().filter_map(Result::ok).collect();
    let paged_targets = targets.clone();
    let started = std::time::Instant::now();
    let paged = tokio::task::spawn_blocking(move || bazel_analysis::paging::page_out(paged_targets.iter().map(|target| target.paged_value()))).await;
    let page_out_time = started.elapsed();
    let paged = match paged {
        Ok(Ok(paged)) => paged,
        Ok(Err(error)) => {
            out.error(&format!("paging out failed: {error:#}")).await;
            return exit::BUILD_FAILURE;
        }
        Err(error) => {
            out.error(&format!("paging out failed: {error}")).await;
            return exit::BUILD_FAILURE;
        }
    };
    let started = std::time::Instant::now();
    let read_back = tokio::task::spawn_blocking(move || {
        let mut failures: std::collections::BTreeMap<String, (usize, String)> = std::collections::BTreeMap::new();
        let mut read = 0;
        for target in &targets {
            match target.providers() {
                Ok(providers) if providers.get().is_some() => read += 1,
                Ok(_) => failures.entry("no providers".to_string()).or_insert((0, target.label.to_string())).0 += 1,
                Err(error) => {
                    let reason = error.message().lines().next().unwrap_or_default().to_string();
                    failures.entry(reason).or_insert((0, target.label.to_string())).0 += 1;
                }
            }
        }
        (read, failures)
    })
    .await;
    let Ok((read, failures)) = read_back else {
        out.error("reading back panicked").await;
        return exit::BUILD_FAILURE;
    };
    let mut text = format!("{paged} targets paged out in {page_out_time:?}; {read} read back in {:?}\n", started.elapsed());
    for (reason, (count, example)) in &failures {
        text.push_str(&format!("{count:>6}  {reason}  (e.g. {example})\n"));
    }
    out.stdout(&text).await;
    exit::SUCCESS
}

/// `dump`: what the server holds in memory; with `--allocator`, the
/// allocator's full report instead.
pub async fn dump(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    if args.iter().any(|arg| arg == "--allocator") {
        out.stdout(&crate::memory::allocator_report()).await;
        return exit::SUCCESS;
    }
    let stats = context.server.workspaces().engine().stats();
    let mut text = String::new();
    text.push_str(&format!(
        "keys: {} ({} bytes)\nmemo: {} keys, {} traces\nstore: {} values, {} live\n  raw {} bytes, stored {} bytes ({:.1}x)\n  pages {} bytes, entries {} bytes, dictionaries {} bytes, live objects {} bytes, zstd contexts {} bytes\ncache: {} entries, {} bytes\n",
        stats.keys,
        stats.key_bytes,
        stats.memo_keys,
        stats.memo_traces,
        stats.store.values,
        stats.store.live_values,
        stats.store.raw_bytes,
        stats.store.stored_bytes,
        stats.store.raw_bytes as f64 / stats.store.stored_bytes.max(1) as f64,
        stats.store.page_bytes,
        stats.store.entry_bytes,
        stats.store.dictionary_bytes,
        stats.store.live_bytes,
        stats.store.context_bytes,
        stats.cache.entries,
        stats.cache.bytes,
    ));
    if let Some(allocator) = crate::memory::allocator_stats() {
        text.push_str(&format!(
            "allocator: {} bytes allocated, {} active, {} metadata, {} resident, {} mapped\n",
            allocator.allocated, allocator.active, allocator.metadata, allocator.resident, allocator.mapped
        ));
    }
    text.push_str("\nkind                          computed  verified    reused     values  raw bytes  stored bytes  compute ms\n");
    for kind in &stats.kinds {
        text.push_str(&format!(
            "{:<28} {:>9} {:>9} {:>9} {:>10} {:>10} {:>13} {:>11}\n",
            kind.name,
            kind.computed,
            kind.verified,
            kind.reused,
            kind.values.values,
            kind.values.raw_bytes,
            kind.values.stored_bytes,
            kind.compute_time.as_millis()
        ));
    }
    text.push_str("\nworkspaces:\n");
    for root in context.server.workspaces().roots() {
        let graph = context.server.workspaces().get(&root).graph.stats();
        text.push_str(&format!("  {} : {} nodes ({} bytes), revision {}\n", root.display(), graph.nodes, graph.node_bytes, graph.revision));
    }
    out.stdout(&text).await;
    exit::SUCCESS
}
