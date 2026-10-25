//! `bb server`: inspect and control the local server.

use crate::actions::{self, ActionsArgs};
use crate::client::ClientArgs;
use crate::daemon::Daemon;
use crate::format;
use admin_rust_connect::buildbox::admin::v1::StorageUsage;
use bazel_rust_connect::buildbox::bazel::v1::{EngineMetrics, GetServerRequest, GetServerResponse};
use bazel_server::PRODUCT;
use clap::{Args, Subcommand};
use std::error::Error;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::time::Duration;

/// How much of each command line `commands` shows.
const MAX_COMMAND_WIDTH: usize = 80;

/// How often `logs --follow` checks for more.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Args, Debug)]
pub struct ServerArgs {
    #[command(subcommand)]
    command: ServerCommand,
}

#[derive(Subcommand, Debug)]
enum ServerCommand {
    /// Summarise the server: its build, storage, load and commands.
    Status(ClientArgs),

    /// List the commands the server is running.
    Commands(ClientArgs),

    /// Cancel commands the server is running.
    Cancel(CancelArgs),

    /// Show what the server holds in memory, by kind of value.
    Metrics(ClientArgs),

    /// List the actions the server is running or has queued.
    Actions(ActionsArgs),

    /// Print the log of a server started in the background.
    Logs(LogsArgs),

    /// List the servers: the default one and those with names, or with
    /// --all, every server running on this machine.
    List(ListArgs),

    /// Start the server in the background, unless it's running.
    Start(SelectionArgs),

    /// Stop the server, once the commands it's running have finished.
    Stop(StopArgs),

    /// Stop the server, once its commands have finished, and start it again.
    Restart(StopArgs),
}

#[derive(Args, Debug)]
struct CancelArgs {
    /// The ids of the commands to cancel, as `commands` lists them.
    #[arg(required_unless_present = "all")]
    ids: Vec<String>,

    /// Cancel every command.
    #[arg(long, conflicts_with = "ids")]
    all: bool,

    #[command(flatten)]
    client: ClientArgs,
}

#[derive(Args, Debug)]
struct LogsArgs {
    /// How many lines from the end to print.
    #[arg(long, short = 'n', default_value_t = 50)]
    lines: usize,

    /// Keep printing what the server logs.
    #[arg(long, short = 'f')]
    follow: bool,

    #[command(flatten)]
    server: SelectionArgs,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// List every server running on this machine, whatever config started
    /// it.
    #[arg(long)]
    all: bool,

    /// Print only how many servers are running on this machine.
    #[arg(long)]
    count: bool,

    #[command(flatten)]
    config: ConfigFileArgs,
}

#[derive(Args, Debug)]
struct ConfigFileArgs {
    /// The config file to read the servers' settings from.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

/// Which server a command is about.
#[derive(Args, Debug)]
struct SelectionArgs {
    /// The server's name. Defaults to $BB_SERVER_NAME, or the default
    /// server.
    #[arg(long = "server-name", value_name = "NAME")]
    server_name: Option<String>,

    #[command(flatten)]
    config: ConfigFileArgs,
}

impl SelectionArgs {
    fn daemon(&self) -> Result<Daemon, Box<dyn Error>> {
        Daemon::new(self.config.config.as_deref(), self.server_name.as_deref())
    }
}

#[derive(Args, Debug)]
struct StopArgs {
    /// Cancel the commands the server is running, rather than waiting for
    /// them to finish.
    #[arg(long)]
    now: bool,

    /// Stop every server that's running.
    #[arg(long, conflicts_with = "server_name")]
    all: bool,

    #[command(flatten)]
    server: SelectionArgs,
}

pub async fn run(args: ServerArgs) -> Result<(), Box<dyn Error>> {
    match args.command {
        ServerCommand::Status(client) => status(client).await,
        ServerCommand::Commands(client) => commands(client).await,
        ServerCommand::Cancel(args) => cancel(args).await,
        ServerCommand::Metrics(client) => metrics(client).await,
        ServerCommand::Actions(args) => actions::run(args).await,
        ServerCommand::Logs(args) => logs(args).await,
        ServerCommand::List(args) => list(args).await,
        ServerCommand::Start(args) => args.daemon()?.ensure_running().await,
        ServerCommand::Stop(args) => stop(args).await,
        ServerCommand::Restart(args) => restart(args).await,
    }
}

async fn status(client: ClientArgs) -> Result<(), Box<dyn Error>> {
    let server = client.connect_server()?.get_server().await?;
    let status = client.connect()?.get_status().await?;
    let uptime = format::duration(Duration::from_secs(server.uptime_seconds));
    let build = format::truncate(&server.build_id, 12);
    println!("{PRODUCT} {} (build {build}), pid {}, up {uptime}", server.version, server.pid);
    if server.shutting_down {
        println!("Stopping once its commands have finished.");
    }
    if let Some(cas) = status.cas.as_option() {
        println!("{:<14}{}", "CAS", storage_line(cas, "blobs"));
    }
    if let Some(action_cache) = status.action_cache.as_option() {
        println!("{:<14}{}", "Action cache", storage_line(action_cache, "entries"));
    }
    if let Some(artifacts) = status.artifacts.as_option() {
        println!("{:<14}{}", "Artifacts", storage_line(artifacts, "artifacts"));
    }
    if let Some(execution) = status.execution.as_option() {
        println!(
            "{:<14}{} running, {} queued, limit {}",
            "Actions", execution.running_actions, execution.queued_actions, execution.max_concurrent_actions
        );
    }
    println!("{:<14}{} running", "Commands", server.commands.len());
    let nodes: u64 = server.workspaces.iter().map(|workspace| workspace.nodes).sum();
    println!("{:<14}{} with {nodes} keys", "Workspaces", server.workspaces.len());
    if let Some(memory) = server.memory.as_option().filter(|memory| memory.resident_bytes > 0) {
        println!(
            "{:<14}{} allocated, {} resident",
            "Memory",
            format::bytes(memory.allocated_bytes),
            format::bytes(memory.resident_bytes)
        );
    }
    Ok(())
}

fn storage_line(usage: &StorageUsage, noun: &str) -> String {
    let summary = format!("{} {noun}, {}", usage.entries, format::bytes(usage.bytes));
    format!("{summary:<26}{}", usage.location)
}

async fn commands(client: ClientArgs) -> Result<(), Box<dyn Error>> {
    let server = client.connect_server()?.get_server().await?;
    if server.commands.is_empty() {
        println!("No commands are running.");
        return Ok(());
    }
    println!("{:<8}{:<10}{:<40}COMMAND", "ID", "TIME", "WORKSPACE");
    for command in &server.commands {
        let time = format::duration(Duration::from_millis(command.running_millis));
        let directory = format::truncate(&command.working_directory, 39);
        let line = format::truncate(&command.args.join(" "), MAX_COMMAND_WIDTH);
        println!("{:<8}{time:<10}{directory:<40}{line}", command.invocation_id);
    }
    Ok(())
}

async fn cancel(args: CancelArgs) -> Result<(), Box<dyn Error>> {
    let control = args.client.connect_server()?;
    let ids = match args.all {
        true => control.get_server().await?.commands.into_iter().map(|command| command.invocation_id).collect(),
        false => args.ids,
    };
    let mut missing = Vec::new();
    for id in &ids {
        if !control.cancel(id).await? {
            missing.push(id.as_str());
        }
    }
    if !missing.is_empty() {
        return Err(format!("no command is running with id {}", missing.join(", ")).into());
    }
    Ok(())
}

async fn metrics(client: ClientArgs) -> Result<(), Box<dyn Error>> {
    let server = client.connect_server()?.get_server().await?;
    if let Some(engine) = server.engine.as_option() {
        print_engine(engine);
    }
    print_workspaces(&server);
    if let Some(memory) = server.memory.as_option().filter(|memory| memory.mapped_bytes > 0) {
        println!(
            "\nallocator: {} allocated, {} active, {} resident, {} mapped",
            format::bytes(memory.allocated_bytes),
            format::bytes(memory.active_bytes),
            format::bytes(memory.resident_bytes),
            format::bytes(memory.mapped_bytes)
        );
    }
    Ok(())
}

fn print_engine(engine: &EngineMetrics) {
    let ratio = engine.raw_bytes as f64 / engine.stored_bytes.max(1) as f64;
    println!("keys: {} ({})", engine.keys, format::bytes(engine.key_bytes));
    println!("memo: {} keys, {} traces", engine.memo_keys, engine.memo_traces);
    println!(
        "values: {} stored, {} live, {} raw, {} stored ({ratio:.1}x)",
        engine.values,
        engine.live_values,
        format::bytes(engine.raw_bytes),
        format::bytes(engine.stored_bytes)
    );
    println!("cache: {} values, {}", engine.cache_entries, format::bytes(engine.cache_bytes));
    println!(
        "\n{:<28} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>11}",
        "KIND", "COMPUTED", "VERIFIED", "REUSED", "VALUES", "RAW", "STORED", "COMPUTE MS"
    );
    for kind in &engine.kinds {
        println!(
            "{:<28} {:>9} {:>9} {:>9} {:>10} {:>10} {:>10} {:>11}",
            kind.name,
            kind.computed,
            kind.verified,
            kind.reused,
            kind.values,
            format::bytes(kind.raw_bytes),
            format::bytes(kind.stored_bytes),
            kind.compute_millis
        );
    }
}

fn print_workspaces(server: &GetServerResponse) {
    println!("\n{:<10} {:>10} {:>9}  ROOT", "NODES", "BYTES", "REVISION");
    for workspace in &server.workspaces {
        println!("{:<10} {:>10} {:>9}  {}", workspace.nodes, format::bytes(workspace.node_bytes), workspace.revision, workspace.root);
    }
}

/// Print the end of the log, and with `--follow`, what's logged after.
async fn logs(args: LogsArgs) -> Result<(), Box<dyn Error>> {
    let path = args.server.daemon()?.log_path();
    let file = std::fs::File::open(&path).map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.by_ref().lines() {
        lines.push(line?);
    }
    for line in &lines[lines.len().saturating_sub(args.lines)..] {
        println!("{line}");
    }
    if !args.follow {
        return Ok(());
    }
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            tokio::time::sleep(FOLLOW_INTERVAL).await;
            continue;
        }
        print!("{line}");
    }
}

/// Print each server, whether it's running, and if so, its pid and what
/// it's doing.
async fn list(args: ListArgs) -> Result<(), Box<dyn Error>> {
    if args.count {
        println!("{}", crate::servers::find().len());
        return Ok(());
    }
    if args.all {
        return list_all().await;
    }
    let args = args.config;
    println!("{:<20}{:<10}{:<10}{:<10}{:<10}URL", "NAME", "STATE", "PID", "UPTIME", "COMMANDS");
    for daemon in Daemon::all(args.config.as_deref())? {
        println!("{}", server_line(&daemon).await);
    }
    Ok(())
}

/// Print every server running on this machine, and how many there are.
async fn list_all() -> Result<(), Box<dyn Error>> {
    let servers = crate::servers::find();
    println!("{}", crate::servers::HEADING);
    for server in &servers {
        println!("{}", crate::servers::line(server).await);
    }
    let noun = if servers.len() == 1 { "server" } else { "servers" };
    println!("{} {noun} running.", servers.len());
    Ok(())
}

async fn server_line(daemon: &Daemon) -> String {
    let name = daemon.name();
    if !daemon.is_running().await {
        return format!("{name:<20}stopped");
    }
    let url = daemon.url().unwrap_or_default();
    let described = match daemon.client() {
        Ok(client) => client.get_server(GetServerRequest::default()).await.ok().map(|server| server.into_owned()),
        Err(_) => None,
    };
    let Some(server) = described else {
        return format!("{name:<20}{:<10}{:<10}{:<10}{:<10}{url}", "running", "?", "?", "?");
    };
    let uptime = format::duration(Duration::from_secs(server.uptime_seconds));
    let state = if server.shutting_down { "stopping" } else { "running" };
    format!("{name:<20}{state:<10}{:<10}{uptime:<10}{:<10}{url}", server.pid, server.commands.len())
}

/// Stop the server, or with `--all`, every server that's running.
async fn stop(args: StopArgs) -> Result<(), Box<dyn Error>> {
    if !args.all {
        return args.server.daemon()?.stop(args.now).await;
    }
    for daemon in running(Daemon::all(args.server.config.config.as_deref())?).await {
        daemon.stop(args.now).await?;
        println!("Stopped {}.", daemon.name());
    }
    Ok(())
}

/// Restart the server, or with `--all`, every server that's running.
async fn restart(args: StopArgs) -> Result<(), Box<dyn Error>> {
    let daemons = match args.all {
        true => running(Daemon::all(args.server.config.config.as_deref())?).await,
        false => vec![args.server.daemon()?],
    };
    for daemon in daemons {
        daemon.stop(args.now).await?;
        daemon.ensure_running().await?;
    }
    Ok(())
}

/// The servers of `daemons` that are running.
async fn running(daemons: Vec<Daemon>) -> Vec<Daemon> {
    let mut running = Vec::new();
    for daemon in daemons {
        if daemon.is_running().await {
            running.push(daemon);
        }
    }
    running
}
