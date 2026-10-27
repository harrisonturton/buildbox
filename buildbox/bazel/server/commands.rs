//! The commands bb implements.

use crate::command::{CommandContext, PRODUCT, VERSION, exit};
use crate::output::Output;
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use std::path::PathBuf;

pub async fn version(args: &[String], out: &Output) -> i32 {
    if args.iter().any(|arg| arg == "--gnu_format") {
        out.stdout(&format!("{PRODUCT} {VERSION}\n")).await;
        return exit::SUCCESS;
    }
    out.stdout(&format!(
        "Build label: {VERSION}\nBuild target: //buildbox/cli:buildbox\nBuild time: Thu Jan 01 00:00:00 1970 (0)\nBuild timestamp: Thu Jan 01 00:00:00 1970 (0)\nBuild timestamp as int: 0\n"
    ))
    .await;
    exit::SUCCESS
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
    ("output_base", "A directory for shared bb state as well as tool and strategy specific subdirectories."),
    ("output_path", "Output directory"),
    ("package_path", "The search path for resolving package labels."),
    ("release", "bb release identifier"),
    ("repository_cache", "The location of the repository download cache used"),
    ("server_pid", "bb process id"),
    (
        "used-heap-size",
        "The amount of used memory in bytes. Note that this is not a good indicator of the actual memory use, as it includes any remaining inaccessible memory.",
    ),
    ("workspace", "The working directory of the server."),
];

/// `bb info`: the values of the keys the command line names, or of all of
/// them, for the configuration its options give.
pub async fn info(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let options = match crate::build::parse(context, "info", args, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let keys = &options.residue;
    if let Some(key) = keys.iter().find(|key| !INFO_KEYS.iter().any(|(name, _)| name == key)) {
        out.error(&format!("unknown key: '{key}'")).await;
        return exit::COMMAND_LINE_ERROR;
    }
    let output_directory = match info_output_directory(context, &options, keys).await {
        Ok(directory) => directory,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let entries = info_values(context, &output_directory);
    let text = match keys.as_slice() {
        [] => entries.iter().map(|(key, value)| format!("{key}: {value}\n")).collect(),
        [key] => format!("{}\n", info_value(&entries, key)),
        keys => keys.iter().map(|key| format!("{key}: {}\n", info_value(&entries, key))).collect(),
    };
    out.stdout(&text).await;
    exit::SUCCESS
}

/// The keys whose values depend on the configuration.
const CONFIGURED_INFO_KEYS: &[&str] = &["bazel-bin", "bazel-genfiles", "bazel-testlogs"];

/// The output directory of the options' configuration, if any of `keys`
/// needs it; it's only worked out then, since that loads the workspace's
/// modules.
async fn info_output_directory(context: &CommandContext<'_>, options: &ParsedOptions, keys: &[String]) -> Result<String, String> {
    let needed = keys.is_empty() || keys.iter().any(|key| CONFIGURED_INFO_KEYS.contains(&key.as_str()));
    if !needed {
        return Ok(String::new());
    }
    let workspace = context.server.workspaces().get(&context.workspace_root);
    workspace.refresh(crate::command::repo_environment(context, options)).await;
    let session = workspace.graph.session().await;
    crate::build::output_directory(context, &session, options).await
}

fn info_value<'a>(entries: &'a [(&'static str, String)], key: &str) -> &'a str {
    let entry = entries.iter().find(|(name, _)| *name == key);
    entry.map(|(_, value)| value.as_str()).unwrap_or_default()
}

/// The value of each of [`INFO_KEYS`], in order, with configured outputs
/// in `output_directory`.
fn info_values(context: &CommandContext<'_>, output_directory: &str) -> Vec<(&'static str, String)> {
    let output_base = output_base(context);
    let execution_root = output_base.join("execroot/_main");
    let output_path = execution_root.join("bazel-out");
    let configured = output_path.join(output_directory);
    let bazel_dir = &context.server.config().bazel_dir;
    let engine = context.server.workspaces().engine().stats();
    let value = |key: &str| match key {
        "bazel-bin" | "bazel-genfiles" => configured.join("bin").display().to_string(),
        "bazel-testlogs" => configured.join("testlogs").display().to_string(),
        "execution_root" => execution_root.display().to_string(),
        "install_base" => bazel_dir.join("install").display().to_string(),
        "output_base" => output_base.display().to_string(),
        "output_path" => output_path.display().to_string(),
        "package_path" => "%workspace%".to_string(),
        "release" => format!("release {VERSION}"),
        "repository_cache" => context.server.config().repository_cache.display().to_string(),
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
    let mut spec = crate::build::build_spec().options();
    spec.extend([("expunge", OptionKind::Bool), ("expunge_async", OptionKind::Bool), ("async", OptionKind::Bool)]);
    let options = match crate::queries::parse_options(context, "clean", args, &OptionSpec::new(&spec), out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let output_base = output_base(context);
    let expunge = options.flag("expunge", false) || options.flag("expunge_async", false);
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
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_back_one(target)));
            let reason = match outcome {
                Ok(Ok(())) => {
                    read += 1;
                    continue;
                }
                Ok(Err(reason)) => reason,
                Err(panic) => format!("panicked: {}", panic_message(&panic)),
            };
            failures.entry(reason).or_insert((0, target.label.to_string())).0 += 1;
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

/// Read back one paged-out target's providers, or say why it can't be.
fn read_back_one(target: &bazel_analysis::ConfiguredTarget) -> Result<(), String> {
    match target.providers() {
        Ok(providers) if providers.get().is_some() => Ok(()),
        Ok(_) => Err("no providers".to_string()),
        Err(error) => Err(error.message().lines().next().unwrap_or_default().to_string()),
    }
}

/// What a caught panic said.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<String>() {
        return message.clone();
    }
    panic.downcast_ref::<&str>().map_or_else(|| "(no message)".to_string(), |message| message.to_string())
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
