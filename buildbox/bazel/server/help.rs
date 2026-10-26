//! `help`: Bazel's help pages, for the commands and options bb supports.

use crate::command::{PRODUCT, VERSION, exit};
use crate::output::Output;
use bazel_options::{OptionKind, OptionSpec};

/// A Bazel command bb runs, as `help` lists it.
struct CommandHelp {
    name: &'static str,
    summary: &'static str,
    /// The command's help page, from Bazel.
    page: &'static str,
}

/// The Bazel commands bb supports, in the order `help` lists them.
const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        name: "build",
        summary: "Builds the specified targets.",
        page: include_str!("help/build.txt"),
    },
    CommandHelp {
        name: "clean",
        summary: "Removes output files and optionally stops the server.",
        page: include_str!("help/clean.txt"),
    },
    CommandHelp {
        name: "dump",
        summary: "Dumps the internal state of the bb server process.",
        page: include_str!("help/dump.txt"),
    },
    CommandHelp {
        name: "help",
        summary: "Prints help for commands, or the index.",
        page: include_str!("help/help.txt"),
    },
    CommandHelp {
        name: "info",
        summary: "Displays runtime info about the bb server.",
        page: include_str!("help/info.txt"),
    },
    CommandHelp {
        name: "license",
        summary: "Prints the license of this software.",
        page: include_str!("help/license.txt"),
    },
    CommandHelp {
        name: "query",
        summary: "Executes a dependency graph query.",
        page: include_str!("help/query.txt"),
    },
    CommandHelp {
        name: "run",
        summary: "Runs the specified target.",
        page: include_str!("help/run.txt"),
    },
    CommandHelp {
        name: "shutdown",
        summary: "Frees the memory this workspace's build graph uses.",
        page: include_str!("help/shutdown.txt"),
    },
    CommandHelp {
        name: "test",
        summary: "Builds and runs the specified test targets.",
        page: include_str!("help/test.txt"),
    },
    CommandHelp {
        name: "version",
        summary: "Prints version information for bb.",
        page: include_str!("help/version.txt"),
    },
];

/// bb's own commands: the server runs `page-check`, and the client the
/// others.
const BB_COMMANDS: &[(&str, &str)] = &[
    ("artifacts", "Manages the server's artifacts, such as container images."),
    ("page-check", "Pages every analyzed target out of memory and back, reporting failures."),
    ("server", "Inspects and controls the server: status, commands, metrics, logs, stop."),
    ("up", "Runs the server in the foreground."),
];

/// The startup options bb reads, as `help startup_options` lists them.
const STARTUP_OPTIONS: &[(&str, &str)] = &[
    ("--bazelrc", "(a string; may be used multiple times)"),
    ("--[no]ignore_all_rc_files", "(a boolean; default: \"false\")"),
];

const STARTUP_OPTIONS_PAGE: &str = "\
Startup options
===============

These options affect how bb starts up, and how it reads the options
of the command that follows. These options must be specified to the
left of the bb command (e.g. 'build'), and they must not contain any
space between option name and value.

Example:
  % bb --bazelrc=/path/to/rc build //base
";

/// The commands of bb's own that the server runs, in a workspace as Bazel
/// commands are.
const BB_SERVER_COMMANDS: &[&str] = &["page-check"];

/// Whether the server runs a command: a Bazel command bb supports, or one
/// of bb's own diagnostics.
pub fn is_supported(command: &str) -> bool {
    COMMANDS.iter().any(|help| help.name == command) || BB_SERVER_COMMANDS.contains(&command)
}

/// `help [topic]`, which Bazel also runs for `-h`, `-help` and `--help`.
pub async fn help(args: &[String], out: &Output) -> i32 {
    let topic = args.iter().find(|arg| !arg.starts_with('-'));
    let Some(topic) = topic else {
        out.stdout(&index()).await;
        return exit::SUCCESS;
    };
    let page = match topic.as_str() {
        "startup_options" => Some(startup_options()),
        "target-syntax" => Some(format!("{}{}", header(), include_str!("help/target-syntax.txt"))),
        "info-keys" => Some(info_keys()),
        command => command_page(command),
    };
    match page {
        Some(page) => {
            out.stdout(&page).await;
            exit::SUCCESS
        }
        None => {
            out.error(&format!("'{topic}' is not a known command")).await;
            exit::COMMAND_LINE_ERROR
        }
    }
}

/// The line every help page starts with.
fn header() -> String {
    format!("{:>80}\n", format!("[{PRODUCT} release {VERSION}]"))
}

/// The list of commands, which `help` prints with no topic.
pub fn index() -> String {
    let mut text = header();
    text.push_str("Usage: bb <command> <options> ...\n\nAvailable commands:\n");
    for command in COMMANDS {
        text.push_str(&format!("  {:<18}  {}\n", command.name, command.summary));
    }
    text.push_str("\nBuildbox commands:\n");
    for (name, summary) in BB_COMMANDS {
        text.push_str(&format!("  {name:<18}  {summary}\n"));
    }
    text.push_str(
        "\nGetting more help:\n  \
         bb help <command>\n                   Prints help and options for <command>.\n  \
         bb help startup_options\n                   Options that affect how bb starts.\n  \
         bb help target-syntax\n                   Explains the syntax for specifying targets.\n  \
         bb help info-keys\n                   Displays a list of keys used by the info command.\n",
    );
    text
}

fn command_page(name: &str) -> Option<String> {
    let command = COMMANDS.iter().find(|command| command.name == name)?;
    let mut text = header();
    text.push_str(command.page);
    if let Some(spec) = command_options(name) {
        text.push_str("\nOptions:\n");
        text.push_str(&option_list(&spec));
    }
    Some(text)
}

/// The options a command's help page lists: those bb reads.
fn command_options(command: &str) -> Option<OptionSpec> {
    match command {
        "build" | "test" | "run" => Some(crate::build::build_spec()),
        "query" => Some(crate::commands::query_spec()),
        _ => None,
    }
}

fn option_list(spec: &OptionSpec) -> String {
    let mut text = String::new();
    for (name, kind) in spec.options() {
        let line = match kind {
            OptionKind::Bool => format!("  --[no]{name}\n"),
            OptionKind::Value | OptionKind::Int | OptionKind::Jobs => format!("  --{name}\n"),
            OptionKind::Multi => format!("  --{name} (may be used multiple times)\n"),
        };
        text.push_str(&line);
    }
    text
}

fn startup_options() -> String {
    let mut text = header();
    text.push_str(STARTUP_OPTIONS_PAGE);
    text.push_str("\nOptions that appear before the command and are parsed by the client:\n");
    for (name, kind) in STARTUP_OPTIONS {
        text.push_str(&format!("  {name} {kind}\n"));
    }
    text
}

fn info_keys() -> String {
    let mut text = String::new();
    for (key, description) in crate::commands::INFO_KEYS {
        text.push_str(&format!("{key:<24}{description}\n"));
    }
    text
}
