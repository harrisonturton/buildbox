//! `bb run`: build an executable target, then have the client run it.

use crate::build::{self, Built};
use crate::command::{CommandContext, exit};
use crate::output::Output;
use bazel_analysis::FileValue;
use bazel_options::ParsedOptions;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Instant;

pub async fn run(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let started = Instant::now();
    let mut options = match build::parse(context, "run", args, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let Some(pattern) = options.residue.first().cloned() else {
        out.error("Must specify at least one target to run").await;
        return exit::COMMAND_LINE_ERROR;
    };
    let program_args = options.residue.split_off(1);
    let workspace = context.server.workspaces().get(&context.workspace_root);
    if let Err(message) = crate::command::refresh(context, &workspace, &options).await {
        out.error(&message).await;
        return crate::command::exit::COMMAND_LINE_ERROR;
    }
    if let Err(code) = build::update_status(context, &workspace, &options, out).await {
        return build::failed(code, started, out).await;
    }
    let session = workspace.graph.session().await;
    let built = match build::run(context, &session, &options, out).await {
        Ok(built) => built,
        Err(code) => return build::failed(code, started, out).await,
    };
    build::report(context, &options, &built, out).await;
    build::report_stats(started, out).await;
    if built.errors {
        out.error("Build failed. Not running target").await;
        return exit::BUILD_FAILURE;
    }
    build::completed(0, out).await;
    let argv = match command_line(&built, &pattern, program_args) {
        Ok(argv) => argv,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let mut shown: Vec<String> = argv.iter().map(|arg| shell_quote(arg)).collect();
    shown[0] = shown_executable(&built, &options);
    out.info(&format!("Running command line: {}", shown.join(" "))).await;
    let working_directory = PathBuf::from(format!("{}.runfiles/_main", argv[0]));
    out.exec(argv, &working_directory, environment(context, &built, &options)).await;
    exit::SUCCESS
}

/// The executable as Bazel shows it, through the `bazel-bin` symlink.
fn shown_executable(built: &Built, options: &ParsedOptions) -> String {
    let prefix = options.get("symlink_prefix").unwrap_or("bazel-");
    let config = built.config.get();
    let executable = built
        .targets
        .first()
        .and_then(|(_, target, _)| {
            let providers = target.providers().ok()?;
            FileValue::artifact(providers.get()?.default_info("executable")?)
        });
    match executable {
        Some(artifact) => build::display_path(&artifact, prefix, &config.output_directory),
        None => String::new(),
    }
}

/// The executable of the one target built, with its `args` and the
/// command line's.
fn command_line(built: &Built, pattern: &str, program_args: Vec<String>) -> Result<Vec<String>, String> {
    let [(label, target, _)] = built.targets.as_slice() else {
        return Err(format!("Only a single target can be run. Your target pattern {pattern} matches {} targets", built.targets.len()));
    };
    let shown = bazel_query::display_label(*label, &built.main_mapping);
    let providers = target.providers().map_err(|error| error.message().to_string())?;
    let executable = providers
        .get()
        .and_then(|value| value.default_info("executable"))
        .and_then(FileValue::artifact)
        .ok_or_else(|| format!("Cannot run target {shown}: Not executable"))?;
    let mut argv = vec![built.exec_root.join(executable.exec_path.to_string()).display().to_string()];
    argv.extend(target.args.iter().cloned());
    argv.extend(program_args);
    Ok(argv)
}

/// Variables `bazel run` always clears, since runfiles libraries would
/// take them as the location of the executable's runfiles.
const CLEARED: [&str; 5] = ["JAVA_RUNFILES", "RUNFILES_DIR", "RUNFILES_MANIFEST_FILE", "RUNFILES_MANIFEST_ONLY", "TEST_SRCDIR"];

/// The environment `bazel run` gives the executable: the client's, with
/// the variables Bazel adds, then `--run_env`'s, then the target's
/// `RunEnvironmentInfo`'s, less those it clears.
fn environment(context: &CommandContext<'_>, built: &Built, options: &ParsedOptions) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = context.env.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    let added = [
        ("BUILD_WORKSPACE_DIRECTORY", context.workspace_root.display().to_string()),
        ("BUILD_WORKING_DIRECTORY", context.working_directory.display().to_string()),
        ("BUILD_EXECROOT", built.exec_root.display().to_string()),
        ("BUILD_ID", context.invocation_id.clone()),
    ];
    env.extend(added.into_iter().map(|(name, value)| (name.to_string(), value)));
    for entry in options.all("run_env") {
        apply_run_env(&mut env, &context.env, entry);
    }
    if let Some((_, target, _)) = built.targets.first() {
        env.extend(target.run_env.resolve(|name| context.env.get(name).cloned()));
    }
    for name in CLEARED {
        env.remove(name);
    }
    env.into_iter().collect()
}

/// Apply one `--run_env`: `name=value` sets a variable, `name` takes the
/// client's value or unsets it if the client has none, and `=name` unsets
/// it.
fn apply_run_env(env: &mut BTreeMap<String, String>, client: &HashMap<String, String>, entry: &str) {
    if let Some(name) = entry.strip_prefix('=') {
        env.remove(name);
        return;
    }
    if let Some((name, value)) = entry.split_once('=') {
        env.insert(name.to_string(), value.to_string());
        return;
    }
    match client.get(entry) {
        Some(value) => env.insert(entry.to_string(), value.clone()),
        None => env.remove(entry),
    };
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "@%_-+=:,./".contains(c));
    match safe {
        true => arg.to_string(),
        false => format!("'{}'", arg.replace('\'', "'\\''")),
    }
}
