//! `bb run`: build an executable target, then have the client run it.

use crate::build::{self, Built};
use crate::command::{CommandContext, exit};
use crate::output::Output;
use bazel_analysis::FileValue;
use bazel_packages::{AttrValue, Target};
use bazel_loading::PackageKey;
use intern::Symbol;
use skyframe::Session;
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
    crate::command::refresh(context, &workspace, &options).await;
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
    let argv = match command_line(&session, &built, &pattern, program_args).await {
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
    out.exec(argv, &working_directory, environment(context)).await;
    exit::SUCCESS
}

/// The executable as Bazel shows it, through the `bazel-bin` symlink.
fn shown_executable(built: &Built, options: &bazel_options::ParsedOptions) -> String {
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
async fn command_line(session: &Session, built: &Built, pattern: &str, program_args: Vec<String>) -> Result<Vec<String>, String> {
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
    argv.extend(rule_args(session, *label).await);
    argv.extend(program_args);
    Ok(argv)
}

/// A rule's `args` attribute.
async fn rule_args(session: &Session, label: bazel_label::Label) -> Vec<String> {
    let Ok(package) = session.evaluate(&PackageKey(label.package)).await else {
        return Vec::new();
    };
    match package.0.target(label.name) {
        Some(Target::Rule(rule)) => match rule.attr(Symbol::intern("args")) {
            Some(AttrValue::StringList(args)) => args.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// The client's environment, with the variables `bazel run` adds.
fn environment(context: &CommandContext<'_>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = context.env.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    env.retain(|(name, _)| name != "BUILD_WORKSPACE_DIRECTORY" && name != "BUILD_WORKING_DIRECTORY");
    env.push(("BUILD_WORKSPACE_DIRECTORY".to_string(), context.workspace_root.display().to_string()));
    env.push(("BUILD_WORKING_DIRECTORY".to_string(), context.working_directory.display().to_string()));
    env.sort();
    env
}

fn shell_quote(arg: &str) -> String {
    let safe = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "@%_-+=:,./".contains(c));
    match safe {
        true => arg.to_string(),
        false => format!("'{}'", arg.replace('\'', "'\\''")),
    }
}
