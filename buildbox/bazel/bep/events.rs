//! Build events, made from what bb knows of a command, as Bazel's
//! `build_event_stream.proto` defines them.

pub use build_event_stream_rust_connect::build_event_stream as proto;
use build_event_stream_rust_connect::build_event_stream::__buffa::oneof::build_event::Payload;
use build_event_stream_rust_connect::build_event_stream::__buffa::oneof::build_event_id::Id;
use build_event_stream_rust_connect::build_event_stream::__buffa::oneof::file::File as FileContents;
use build_event_stream_rust_connect::build_event_stream::build_event_id as ids;
use buffa::MessageField;
use buffa_types::google::protobuf::{Duration, Timestamp};
use std::path::Path;
use std::time::SystemTime;

pub use proto::{BuildEvent, BuildEventId};

/// An event with an id, the events it announces, and its payload.
fn event(id: BuildEventId, children: Vec<BuildEventId>, payload: Payload) -> BuildEvent {
    BuildEvent {
        id: MessageField::some(id),
        children,
        payload: Some(payload),
        ..BuildEvent::default()
    }
}

fn id(id: Id) -> BuildEventId {
    BuildEventId {
        id: Some(id),
        ..BuildEventId::default()
    }
}

/// The ids of events, by what they're about.
pub mod id {
    use super::*;

    pub fn started() -> BuildEventId {
        id(Id::Started(Box::default()))
    }

    pub fn unstructured_command_line() -> BuildEventId {
        id(Id::UnstructuredCommandLine(Box::default()))
    }

    pub fn options_parsed() -> BuildEventId {
        id(Id::OptionsParsed(Box::default()))
    }

    pub fn workspace_status() -> BuildEventId {
        id(Id::WorkspaceStatus(Box::default()))
    }

    pub fn workspace_info() -> BuildEventId {
        id(Id::Workspace(Box::default()))
    }

    pub fn build_metadata() -> BuildEventId {
        id(Id::BuildMetadata(Box::default()))
    }

    pub fn pattern(patterns: &[String]) -> BuildEventId {
        id(Id::Pattern(Box::new(ids::PatternExpandedId {
            pattern: patterns.to_vec(),
            ..Default::default()
        })))
    }

    pub fn progress(count: i32) -> BuildEventId {
        id(Id::Progress(Box::new(ids::ProgressId {
            opaque_count: count,
            ..Default::default()
        })))
    }

    pub fn configuration(configuration: &str) -> BuildEventId {
        id(Id::Configuration(Box::new(configuration_id(configuration))))
    }

    pub fn target_configured(label: &str) -> BuildEventId {
        id(Id::TargetConfigured(Box::new(ids::TargetConfiguredId {
            label: label.to_string(),
            ..Default::default()
        })))
    }

    pub fn named_set(set: usize) -> BuildEventId {
        id(Id::NamedSet(Box::new(named_set_id(set))))
    }

    pub fn target_completed(label: &str, configuration: &str) -> BuildEventId {
        id(Id::TargetCompleted(Box::new(ids::TargetCompletedId {
            label: label.to_string(),
            configuration: MessageField::some(configuration_id(configuration)),
            ..Default::default()
        })))
    }

    /// A test's run, shard and attempt, each counted from 1.
    pub fn test_result(label: &str, configuration: &str, run: i32, shard: i32, attempt: i32) -> BuildEventId {
        id(Id::TestResult(Box::new(ids::TestResultId {
            label: label.to_string(),
            configuration: MessageField::some(configuration_id(configuration)),
            run,
            shard,
            attempt,
            ..Default::default()
        })))
    }

    pub fn test_summary(label: &str, configuration: &str) -> BuildEventId {
        id(Id::TestSummary(Box::new(ids::TestSummaryId {
            label: label.to_string(),
            configuration: MessageField::some(configuration_id(configuration)),
            ..Default::default()
        })))
    }

    pub fn build_finished() -> BuildEventId {
        id(Id::BuildFinished(Box::default()))
    }

    pub fn build_tool_logs() -> BuildEventId {
        id(Id::BuildToolLogs(Box::default()))
    }

    pub fn build_metrics() -> BuildEventId {
        id(Id::BuildMetrics(Box::default()))
    }

    pub(super) fn configuration_id(configuration: &str) -> ids::ConfigurationId {
        ids::ConfigurationId {
            id: configuration.to_string(),
            ..Default::default()
        }
    }

    pub(super) fn named_set_id(set: usize) -> ids::NamedSetOfFilesId {
        ids::NamedSetOfFilesId {
            id: set.to_string(),
            ..Default::default()
        }
    }
}

/// What starts every command's events.
pub struct Started<'a> {
    pub uuid: &'a str,
    pub start: SystemTime,
    pub version: &'a str,
    pub options_description: &'a str,
    pub command: &'a str,
    pub working_directory: &'a Path,
    pub workspace_directory: &'a Path,
    pub server_pid: u32,
}

pub fn started(started: &Started<'_>, children: Vec<BuildEventId>) -> BuildEvent {
    let payload = proto::BuildStarted {
        uuid: started.uuid.to_string(),
        start_time_millis: millis(started.start),
        start_time: MessageField::some(Timestamp::from(started.start)),
        build_tool_version: started.version.to_string(),
        options_description: started.options_description.to_string(),
        command: started.command.to_string(),
        working_directory: started.working_directory.display().to_string(),
        workspace_directory: started.workspace_directory.display().to_string(),
        server_pid: i64::from(started.server_pid),
        ..Default::default()
    };
    event(id::started(), children, Payload::Started(Box::new(payload)))
}

/// The command line as the client gave it.
pub fn unstructured_command_line(args: &[String]) -> BuildEvent {
    let payload = proto::UnstructuredCommandLine {
        args: args.to_vec(),
        ..Default::default()
    };
    event(id::unstructured_command_line(), Vec::new(), Payload::UnstructuredCommandLine(Box::new(payload)))
}

/// The options the command runs with: every option, from rc files and the
/// command line, and those given explicitly.
pub fn options_parsed(startup: &[String], command_line: &[String], explicit: &[String]) -> BuildEvent {
    let payload = proto::OptionsParsed {
        startup_options: startup.to_vec(),
        explicit_startup_options: startup.to_vec(),
        cmd_line: command_line.to_vec(),
        explicit_cmd_line: explicit.to_vec(),
        ..Default::default()
    };
    event(id::options_parsed(), Vec::new(), Payload::OptionsParsed(Box::new(payload)))
}

pub fn build_metadata() -> BuildEvent {
    event(id::build_metadata(), Vec::new(), Payload::BuildMetadata(Box::default()))
}

/// The targets patterns expanded to, which the event announces.
pub fn pattern_expanded(patterns: &[String], targets: &[String]) -> BuildEvent {
    let children = targets.iter().map(|target| id::target_configured(target)).collect();
    event(id::pattern(patterns), children, Payload::Expanded(Box::default()))
}

pub fn workspace_info(exec_root: &Path) -> BuildEvent {
    let payload = proto::WorkspaceConfig {
        local_exec_root: exec_root.display().to_string(),
        ..Default::default()
    };
    event(id::workspace_info(), Vec::new(), Payload::WorkspaceInfo(Box::new(payload)))
}

/// A configuration targets are built in.
pub struct Configuration<'a> {
    pub id: &'a str,
    pub mnemonic: &'a str,
    pub platform_name: &'a str,
    pub cpu: &'a str,
    pub make_variables: Vec<(String, String)>,
    pub is_tool: bool,
}

pub fn configuration(configuration: &Configuration<'_>) -> BuildEvent {
    let payload = proto::Configuration {
        mnemonic: configuration.mnemonic.to_string(),
        platform_name: configuration.platform_name.to_string(),
        cpu: configuration.cpu.to_string(),
        make_variable: configuration.make_variables.iter().cloned().collect(),
        is_tool: configuration.is_tool,
        ..Default::default()
    };
    event(id::configuration(configuration.id), Vec::new(), Payload::Configuration(Box::new(payload)))
}

/// The build's status values, from the workspace status command and bb.
pub fn workspace_status(items: &[(String, String)]) -> BuildEvent {
    let item = items
        .iter()
        .map(|(key, value)| proto::workspace_status::Item {
            key: key.clone(),
            value: value.clone(),
            ..Default::default()
        })
        .collect();
    let payload = proto::WorkspaceStatus {
        item,
        ..Default::default()
    };
    event(id::workspace_status(), Vec::new(), Payload::WorkspaceStatus(Box::new(payload)))
}

/// The size a test's `size` gives it.
pub fn test_size(size: &str) -> proto::TestSize {
    match size {
        "small" => proto::TestSize::SMALL,
        "medium" => proto::TestSize::MEDIUM,
        "large" => proto::TestSize::LARGE,
        "enormous" => proto::TestSize::ENORMOUS,
        _ => proto::TestSize::UNKNOWN,
    }
}

/// That a target was configured, announcing its completion.
pub fn target_configured(label: &str, configuration: &str, kind: &str, test_size: Option<proto::TestSize>, tags: &[String]) -> BuildEvent {
    let payload = proto::TargetConfigured {
        target_kind: format!("{kind} rule"),
        test_size: test_size.unwrap_or(proto::TestSize::UNKNOWN).into(),
        tag: tags.to_vec(),
        ..Default::default()
    };
    let children = vec![id::target_completed(label, configuration)];
    event(id::target_configured(label), children, Payload::Configured(Box::new(payload)))
}

/// A file a build event names: by its path on disk, relative to the
/// execution root, which `exec_root` is.
pub struct FileRef<'a> {
    pub exec_path: &'a str,
    pub exec_root: &'a Path,
    /// The root of output files, such as `bazel-out/k8-fastbuild/bin`,
    /// which `path_prefix` gives, empty for source files.
    pub root: &'a str,
    pub digest: Option<String>,
    pub length: Option<u64>,
}

pub fn file(reference: &FileRef<'_>) -> proto::File {
    let name = match reference.root.is_empty() {
        true => reference.exec_path,
        false => reference.exec_path.strip_prefix(reference.root).map(|name| name.trim_start_matches('/')).unwrap_or(reference.exec_path),
    };
    let path_prefix = reference.root.split('/').filter(|part| !part.is_empty()).map(str::to_string).collect();
    proto::File {
        name: name.to_string(),
        file: Some(FileContents::Uri(file_uri(&reference.exec_root.join(reference.exec_path)))),
        path_prefix,
        digest: reference.digest.clone().unwrap_or_default(),
        length: reference.length.map(|length| length as i64).unwrap_or_default(),
        ..Default::default()
    }
}

/// A file given only by its path on disk and its name, as test outputs
/// and logs are.
pub fn file_at(name: &str, path: &Path) -> proto::File {
    proto::File {
        name: name.to_string(),
        file: Some(FileContents::Uri(file_uri(path))),
        ..Default::default()
    }
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

pub fn named_set(set: usize, files: Vec<proto::File>) -> BuildEvent {
    let payload = proto::NamedSetOfFiles {
        files,
        ..Default::default()
    };
    event(id::named_set(set), Vec::new(), Payload::NamedSetOfFiles(Box::new(payload)))
}

/// A target that finished building, or failed to.
pub struct Completed<'a> {
    pub label: &'a str,
    pub configuration: &'a str,
    pub success: bool,
    /// The output groups it built, and the named sets of their files.
    pub output_groups: Vec<(String, Vec<usize>)>,
    pub tags: &'a [String],
    /// A test's timeout.
    pub test_timeout_seconds: Option<u64>,
    /// The events of its tests' results that follow.
    pub children: Vec<BuildEventId>,
}

pub fn target_completed(completed: Completed<'_>) -> BuildEvent {
    let output_group = completed
        .output_groups
        .into_iter()
        .map(|(name, sets)| proto::OutputGroup {
            name,
            file_sets: sets.into_iter().map(id::named_set_id).collect(),
            ..Default::default()
        })
        .collect();
    let payload = proto::TargetComplete {
        success: completed.success,
        output_group,
        tag: completed.tags.to_vec(),
        test_timeout_seconds: completed.test_timeout_seconds.map(|seconds| seconds as i64).unwrap_or_default(),
        test_timeout: match completed.test_timeout_seconds {
            Some(seconds) => MessageField::some(Duration::from_secs(seconds as i64)),
            None => MessageField::none(),
        },
        ..Default::default()
    };
    event(id::target_completed(completed.label, completed.configuration), completed.children, Payload::Completed(Box::new(payload)))
}

/// The status of a test, as the protocol has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStatus {
    NoStatus,
    Passed,
    Flaky,
    TimedOut,
    Failed,
}

impl TestStatus {
    fn proto(self) -> proto::TestStatus {
        match self {
            TestStatus::NoStatus => proto::TestStatus::NO_STATUS,
            TestStatus::Passed => proto::TestStatus::PASSED,
            TestStatus::Flaky => proto::TestStatus::FLAKY,
            TestStatus::TimedOut => proto::TestStatus::TIMEOUT,
            TestStatus::Failed => proto::TestStatus::FAILED,
        }
    }
}

/// One attempt at running one shard of one run of a test.
pub struct TestAttempt<'a> {
    pub label: &'a str,
    pub configuration: &'a str,
    /// Its run, shard and attempt, each counted from 1.
    pub run: i32,
    pub shard: i32,
    pub attempt: i32,
    pub status: TestStatus,
    pub cached: bool,
    pub start: SystemTime,
    pub duration_ms: u64,
    pub exit_code: i32,
    /// `test.log`, `test.xml` and the other files it left.
    pub outputs: Vec<proto::File>,
    pub strategy: &'a str,
}

pub fn test_result(attempt: TestAttempt<'_>) -> BuildEvent {
    let execution_info = proto::test_result::ExecutionInfo {
        strategy: attempt.strategy.to_string(),
        exit_code: attempt.exit_code,
        ..Default::default()
    };
    let payload = proto::TestResult {
        status: attempt.status.proto().into(),
        cached_locally: attempt.cached,
        test_attempt_start_millis_epoch: millis(attempt.start),
        test_attempt_start: MessageField::some(Timestamp::from(attempt.start)),
        test_attempt_duration_millis: attempt.duration_ms as i64,
        test_attempt_duration: MessageField::some(Duration::from_millis(attempt.duration_ms as i64)),
        test_action_output: attempt.outputs,
        execution_info: MessageField::some(execution_info),
        ..Default::default()
    };
    let id = id::test_result(attempt.label, attempt.configuration, attempt.run, attempt.shard, attempt.attempt);
    event(id, Vec::new(), Payload::TestResult(Box::new(payload)))
}

/// A test's results together.
pub struct Summary<'a> {
    pub label: &'a str,
    pub configuration: &'a str,
    pub status: TestStatus,
    pub total_runs: i32,
    pub runs: i32,
    pub shards: i32,
    pub attempts: i32,
    pub cached: i32,
    pub passed: Vec<proto::File>,
    pub failed: Vec<proto::File>,
    pub first_start: SystemTime,
    pub last_stop: SystemTime,
    pub total_duration_ms: u64,
}

pub fn test_summary(summary: Summary<'_>) -> BuildEvent {
    let payload = proto::TestSummary {
        overall_status: summary.status.proto().into(),
        total_run_count: summary.total_runs,
        run_count: summary.runs,
        attempt_count: summary.attempts,
        shard_count: summary.shards,
        passed: summary.passed,
        failed: summary.failed,
        total_num_cached: summary.cached,
        first_start_time_millis: millis(summary.first_start),
        first_start_time: MessageField::some(Timestamp::from(summary.first_start)),
        last_stop_time_millis: millis(summary.last_stop),
        last_stop_time: MessageField::some(Timestamp::from(summary.last_stop)),
        total_run_duration_millis: summary.total_duration_ms as i64,
        total_run_duration: MessageField::some(Duration::from_millis(summary.total_duration_ms as i64)),
        ..Default::default()
    };
    event(id::test_summary(summary.label, summary.configuration), Vec::new(), Payload::TestSummary(Box::new(payload)))
}

/// That the command finished, with Bazel's name and number for its exit
/// code, announcing the logs and metrics that follow.
pub fn build_finished(exit_code: i32, finish: SystemTime) -> BuildEvent {
    let payload = proto::BuildFinished {
        overall_success: exit_code == 0,
        exit_code: MessageField::some(proto::build_finished::ExitCode {
            name: exit_code_name(exit_code).to_string(),
            code: exit_code,
            ..Default::default()
        }),
        finish_time_millis: millis(finish),
        finish_time: MessageField::some(Timestamp::from(finish)),
        ..Default::default()
    };
    let children = vec![id::build_tool_logs(), id::build_metrics()];
    event(id::build_finished(), children, Payload::Finished(Box::new(payload)))
}

/// The name Bazel gives an exit code.
fn exit_code_name(code: i32) -> &'static str {
    match code {
        0 => "SUCCESS",
        1 => "BUILD_FAILURE",
        2 => "COMMAND_LINE_ERROR",
        3 => "TESTS_FAILED",
        4 => "NO_TESTS_FOUND",
        8 => "INTERRUPTED",
        36 => "LOCAL_ENVIRONMENTAL_ERROR",
        37 => "BLAZE_INTERNAL_ERROR",
        _ => "UNKNOWN",
    }
}

/// The logs of the command: how long it took.
pub fn build_tool_logs(elapsed_seconds: f64) -> BuildEvent {
    let log = vec![proto::File {
        name: "elapsed time".to_string(),
        file: Some(FileContents::Contents(format!("{elapsed_seconds:.6}").into_bytes())),
        ..Default::default()
    }];
    let payload = proto::BuildToolLogs {
        log,
        ..Default::default()
    };
    event(id::build_tool_logs(), Vec::new(), Payload::BuildToolLogs(Box::new(payload)))
}

/// What the command did: how many actions it ran, of how many.
pub fn build_metrics(actions_executed: i64, actions_created: i64) -> BuildEvent {
    let summary = proto::build_metrics::ActionSummary {
        actions_executed,
        actions_created,
        ..Default::default()
    };
    let payload = proto::BuildMetrics {
        action_summary: MessageField::some(summary),
        ..Default::default()
    };
    event(id::build_metrics(), Vec::new(), Payload::BuildMetrics(Box::new(payload)))
}

/// That an announced event won't come, because the command stopped
/// before it, as Bazel reports it.
pub fn aborted(id: BuildEventId, description: &str) -> BuildEvent {
    let payload = proto::Aborted {
        reason: proto::aborted::AbortReason::INCOMPLETE.into(),
        description: description.to_string(),
        ..Default::default()
    };
    event(id, Vec::new(), Payload::Aborted(Box::new(payload)))
}

/// Output the command printed, announcing the next progress event, if it
/// isn't the last.
pub fn progress(count: i32, stdout: &str, stderr: &str, last: bool) -> BuildEvent {
    let payload = proto::Progress {
        stdout: stdout.to_string(),
        stderr: stderr.to_string(),
        ..Default::default()
    };
    let children = match last {
        true => Vec::new(),
        false => vec![id::progress(count + 1)],
    };
    event(id::progress(count), children, Payload::Progress(Box::new(payload)))
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or_default()
}
