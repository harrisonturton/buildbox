//! Messages for what happens while a command runs, such as an action
//! failing, worded as Bazel words them.

use crate::Kind;
use bazel_progress::{ActionFailure, ActionOutput, Command, FailureReason};
use std::path::Path;

/// How far a command line in a failure is shown before the rest of its
/// arguments are left out, unless failures are verbose.
const ABBREVIATED_LENGTH: usize = 200;

const SANDBOX_DEBUG_HINT: &str = "Use --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging";

/// What an event shows: messages, then output for stderr and stdout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub messages: Vec<(Kind, String)>,
    pub stderr: String,
    pub stdout: String,
}

/// How failures are shown, from the command's options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FailureFormat {
    /// `--verbose_failures`: show failed commands in full, as scripts.
    pub verbose: bool,
    /// Where commands run, for showing them in full.
    pub exec_root: Option<std::path::PathBuf>,
}

/// What a successful action printed: `INFO: From <description>:`, then its
/// output.
pub fn action_output(output: &ActionOutput) -> Shown {
    Shown {
        messages: vec![(Kind::Info, format!("From {}:", output.description))],
        stderr: output.stderr.clone(),
        stdout: output.stdout.clone(),
    }
}

/// Why an action failed, then what it printed.
pub fn action_failure(failure: &ActionFailure, format: &FailureFormat) -> Shown {
    let prefix = match &failure.location {
        Some(location) => format!("{location}: "),
        None => String::new(),
    };
    let failed = format!("{prefix}{} failed: ", failure.description);
    let messages = match &failure.reason {
        FailureReason::Exit { code, command, sandboxed } => {
            let mut message = format!("{failed}(Exit {code}): {}", describe_command_failure(failure, command, format));
            if *sandboxed {
                message.push_str("\n\n");
                message.push_str(SANDBOX_DEBUG_HINT);
            }
            vec![message]
        }
        FailureReason::MissingOutputs(paths) => {
            let mut messages: Vec<String> = paths.iter().map(|path| format!("{prefix}{}", not_created(&failure.mnemonic, path))).collect();
            messages.push(format!("{failed}not all outputs were created or valid"));
            messages
        }
        FailureReason::MissingInputs(labels) => {
            let mut messages: Vec<String> = labels.iter().map(|label| format!("{failed}missing input file '{label}'")).collect();
            messages.push(format!("{failed}{} input file(s) do not exist", labels.len()));
            messages
        }
    };
    Shown {
        messages: messages.into_iter().map(|message| (Kind::Error, message)).collect(),
        stderr: failure.output.stderr.clone(),
        stdout: failure.output.stdout.clone(),
    }
}

/// An output that wasn't created, with genrules' explanation of why that
/// may be.
fn not_created(mnemonic: &str, path: &str) -> String {
    match mnemonic {
        "Genrule" => format!(
            "declared output '{path}' was not created by genrule. This is probably because the genrule actually didn't create this output, or because the output was a directory and the genrule was run remotely (note that only the contents of declared file outputs are copied from genrules run remotely)"
        ),
        _ => format!("output '{path}' was not created"),
    }
}

/// `bash failed: error executing Genrule command (from genrule rule target
/// //a:b) ...`.
fn describe_command_failure(failure: &ActionFailure, command: &Command, format: &FailureFormat) -> String {
    let program = command.arguments.first().map(String::as_str).unwrap_or_default();
    let short_name = Path::new(program).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut text = format!("{short_name} failed: error executing {} command (from {}) ", failure.mnemonic, failure.owner);
    match format.verbose {
        true => text.push_str(&complete_command(command, format.exec_root.as_deref())),
        false => text.push_str(&abbreviated_command(&command.arguments)),
    }
    text
}

/// A command line, cut short once it's long.
fn abbreviated_command(arguments: &[String]) -> String {
    let mut text = String::new();
    for (index, argument) in arguments.iter().enumerate() {
        let escaped = shell_escape(argument);
        if text.len() + escaped.len() > ABBREVIATED_LENGTH {
            let remaining = arguments.len() - index;
            let noun = if remaining == 1 { "argument" } else { "arguments" };
            text.push_str(&format!(" ... (remaining {remaining} {noun} skipped)"));
            break;
        }
        if index > 0 {
            text.push(' ');
        }
        text.push_str(&escaped);
    }
    text
}

/// A command in full, as a script that runs it as it was run.
fn complete_command(command: &Command, exec_root: Option<&Path>) -> String {
    let mut text = String::from("\n  (");
    if let Some(root) = exec_root {
        text.push_str(&format!("cd {} && \\\n  ", shell_escape(&root.display().to_string())));
    }
    text.push_str("exec env - \\\n");
    for (name, value) in &command.env {
        text.push_str(&format!("    {name}={} \\\n", shell_escape(value)));
    }
    let arguments: Vec<String> = command.arguments.iter().map(|argument| shell_escape(argument)).collect();
    text.push_str(&format!("  {})", arguments.join(" ")));
    text.push_str(&format!("\n# Configuration: {}", command.configuration));
    text.push_str(&format!("\n# Execution platform: {}", command.execution_platform));
    text
}

/// `text` quoted for a POSIX shell, unless it needs no quoting.
fn shell_escape(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(reason: FailureReason) -> ActionFailure {
        ActionFailure {
            error: String::new(),
            location: Some("/w/fail/BUILD.bazel:1:8".to_string()),
            description: "Executing genrule //fail:bad".to_string(),
            owner: "genrule rule target //fail:bad".to_string(),
            mnemonic: "Genrule".to_string(),
            reason,
            output: ActionOutput {
                description: String::new(),
                stdout: "to-stdout\n".to_string(),
                stderr: "oops\n".to_string(),
            },
        }
    }

    fn exit() -> FailureReason {
        FailureReason::Exit {
            code: 3,
            command: Command {
                arguments: vec!["/bin/bash".to_string(), "-c".to_string(), "echo oops >&2; exit 3".to_string()],
                env: vec![("PATH".to_string(), "/bin:/usr/bin".to_string())],
                configuration: "e849".to_string(),
                execution_platform: "@@platforms//host:host".to_string(),
            },
            sandboxed: true,
        }
    }

    #[test]
    fn failed_commands_are_described_as_bazel_does() {
        let shown = action_failure(&failure(exit()), &FailureFormat::default());
        assert_eq!(
            shown.messages,
            [(
                Kind::Error,
                format!(
                    "/w/fail/BUILD.bazel:1:8: Executing genrule //fail:bad failed: (Exit 3): bash failed: error executing Genrule command (from genrule rule target //fail:bad) /bin/bash -c 'echo oops >&2; exit 3'\n\n{SANDBOX_DEBUG_HINT}"
                )
            )]
        );
        assert_eq!((shown.stdout.as_str(), shown.stderr.as_str()), ("to-stdout\n", "oops\n"));
    }

    #[test]
    fn verbose_failures_show_the_command_as_a_script() {
        let format = FailureFormat {
            verbose: true,
            exec_root: Some("/out/execroot/_main".into()),
        };
        let shown = action_failure(&failure(exit()), &format);
        let expected = "error executing Genrule command (from genrule rule target //fail:bad) \n  (cd /out/execroot/_main && \\\n  exec env - \\\n    PATH=/bin:/usr/bin \\\n  /bin/bash -c 'echo oops >&2; exit 3')\n# Configuration: e849\n# Execution platform: @@platforms//host:host";
        assert!(shown.messages[0].1.contains(expected), "{}", shown.messages[0].1);
    }

    #[test]
    fn long_command_lines_are_cut_short() {
        let arguments: Vec<String> = (0..100).map(|index| format!("argument{index}")).collect();
        let text = abbreviated_command(&arguments);
        assert!(text.ends_with(" ... (remaining 81 arguments skipped)"), "{text}");
    }

    #[test]
    fn missing_outputs_and_inputs_are_listed() {
        let shown = action_failure(&failure(FailureReason::MissingOutputs(vec!["fail/bad.txt".to_string()])), &FailureFormat::default());
        assert!(shown.messages[0].1.starts_with("/w/fail/BUILD.bazel:1:8: declared output 'fail/bad.txt' was not created by genrule."));
        assert_eq!(shown.messages[1].1, "/w/fail/BUILD.bazel:1:8: Executing genrule //fail:bad failed: not all outputs were created or valid");

        let shown = action_failure(&failure(FailureReason::MissingInputs(vec!["//fail:in.txt".to_string()])), &FailureFormat::default());
        let messages: Vec<&str> = shown.messages.iter().map(|(_, message)| message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "/w/fail/BUILD.bazel:1:8: Executing genrule //fail:bad failed: missing input file '//fail:in.txt'",
                "/w/fail/BUILD.bazel:1:8: Executing genrule //fail:bad failed: 1 input file(s) do not exist",
            ]
        );
    }
}
