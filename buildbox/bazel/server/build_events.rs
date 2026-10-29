//! A command's build events: opening the files `--build_event_json_file`
//! and `--build_event_binary_file` name, the events every command starts
//! and ends with, and keeping the stream whole, every announced event
//! posted or reported aborted, as Bazel's is.

use crate::command::{CommandContext, PRODUCT, VERSION, exit};
use crate::output::Output;
use bazel_analysis::ConfiguredTarget;
use bazel_analysis::workspace_status::StatusValues;
use bazel_bep::BuildEventLog;
use bazel_bep::events::{self, BuildEvent, BuildEventId, FileRef, Started};
use bazel_config::BuildConfiguration;
use bazel_execution::OutputFile;
use bazel_options::ParsedOptions;
use bazel_packages::{AttrValue, Rule};
use intern::Symbol;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// Start the events of a command whose options ask for them, so that
/// the rest of the command posts to them through `out`.
pub async fn start(context: &CommandContext<'_>, command: &str, args: &[String], options: &ParsedOptions, out: &Output) -> Result<(), i32> {
    let json = options.get("build_event_json_file").filter(|path| !path.is_empty());
    let binary = options.get("build_event_binary_file").filter(|path| !path.is_empty());
    let json = json.map(|path| context.working_directory.join(path));
    let binary = binary.map(|path| context.working_directory.join(path));
    let all_args = bazel_options::command_args(&context.rc, command, args);
    let explicit: Vec<String> = args.iter().take_while(|arg| *arg != "--").filter(|arg| arg.starts_with('-')).cloned().collect();
    let version = format!("{PRODUCT} {VERSION}");
    let mut command_line = vec![PRODUCT.to_string(), command.to_string()];
    command_line.extend(args.iter().cloned());
    let info = CommandInfo {
        started: Started {
            uuid: &context.invocation_id,
            start: SystemTime::now(),
            version: &version,
            options_description: &explicit.join(" "),
            command,
            working_directory: &context.working_directory,
            workspace_directory: &context.workspace_root,
            server_pid: std::process::id(),
        },
        args: command_line,
        options: all_args.into_iter().map(|arg| arg.text).filter(|arg| arg.starts_with('-')).collect(),
        explicit_options: explicit.clone(),
        patterns: options.residue.clone(),
    };
    match BuildEvents::open(json.as_deref(), binary.as_deref(), info).await {
        Ok(Some(events)) => {
            out.set_build_events(Arc::new(events));
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(message) => {
            out.error(&message).await;
            Err(exit::LOCAL_ENVIRONMENTAL_ERROR)
        }
    }
}

/// End the events of a command that wrote them, now that it finished
/// with `exit_code`.
pub async fn finish(exit_code: i32, out: &Output) {
    let Some(events) = out.take_build_events() else {
        return;
    };
    let actions = out.snapshot().await.actions_finished as i64;
    match events.finish(exit_code, actions).await {
        Ok(()) => out.info("Build Event Protocol files produced successfully.").await,
        Err(message) => out.error(&message).await,
    }
}

/// The events of a command that writes them.
pub struct BuildEvents {
    log: BuildEventLog,
    state: Mutex<State>,
    started: Instant,
    /// The target patterns, whose expansion is announced.
    patterns: Vec<String>,
}

#[derive(Default)]
struct State {
    /// The events announced as children of others, in order.
    announced: Vec<BuildEventId>,
    /// Every event posted, by its id's JSON.
    posted: HashSet<String>,
    /// The configurations already described.
    configurations: HashSet<String>,
    /// How many named sets of files have been posted.
    named_sets: usize,
    /// What the protocol says of each target the patterns expanded to.
    targets: HashMap<String, TargetAttrs>,
    stdout: String,
    stderr: String,
}

/// What the protocol says of a target the patterns name, from its rule.
#[derive(Clone, Debug, Default)]
pub struct TargetAttrs {
    pub tags: Vec<String>,
    /// A test's size and timeout in seconds.
    pub test: Option<(String, u64)>,
    /// The tags a rule's taggable attributes add, as Bazel reports them
    /// once the target completes: a test's size, timeout, and whether
    /// it's flaky and local.
    pub implicit_tags: Vec<String>,
}

impl TargetAttrs {
    /// The attributes of `rule`, if the target is one.
    pub fn of(rule: Option<&Rule>, is_test: bool) -> TargetAttrs {
        let Some(rule) = rule else {
            return TargetAttrs::default();
        };
        if !is_test {
            return TargetAttrs { tags: tags(Some(rule)), ..TargetAttrs::default() };
        }
        let size = string_attr(rule, "size").unwrap_or_else(|| "medium".to_string());
        let timeout = test_timeout(rule);
        let implicit_tags = vec![size.clone(), timeout.to_string(), bool_tag(rule, "flaky"), bool_tag(rule, "local")];
        TargetAttrs {
            tags: tags(Some(rule)),
            test: Some((size, timeout_seconds(timeout))),
            implicit_tags,
        }
    }
}

/// What the command is, for the event that starts it.
pub struct CommandInfo<'a> {
    pub started: Started<'a>,
    /// The command line as the client gave it.
    pub args: Vec<String>,
    /// Every option, from rc files and the command line.
    pub options: Vec<String>,
    /// The options on the command line.
    pub explicit_options: Vec<String>,
    /// The target patterns, which a pattern event expands.
    pub patterns: Vec<String>,
}

impl BuildEvents {
    /// Start a command's events, if the options ask for them: the started
    /// event and those describing the command line.
    pub async fn open(json: Option<&Path>, binary: Option<&Path>, info: CommandInfo<'_>) -> Result<Option<BuildEvents>, String> {
        let log = BuildEventLog::open(json.map(Into::into), binary.map(Into::into)).await;
        let log = match log {
            Ok(Some(log)) => log,
            Ok(None) => return Ok(None),
            Err(error) => return Err(format!("Unable to write build events: {error}")),
        };
        let events = BuildEvents {
            log,
            state: Mutex::new(State::default()),
            started: Instant::now(),
            patterns: info.patterns.clone(),
        };
        let mut children = vec![
            events::id::progress(0),
            events::id::unstructured_command_line(),
            events::id::build_metadata(),
            events::id::options_parsed(),
            events::id::workspace_status(),
        ];
        if !info.patterns.is_empty() {
            children.push(events::id::pattern(&info.patterns));
        }
        children.push(events::id::workspace_info());
        children.push(events::id::build_finished());
        events.post(events::started(&info.started, children));
        events.post(events::build_metadata());
        events.post(events::unstructured_command_line(&info.args));
        events.post(events::options_parsed(&[], &info.options, &info.explicit_options));
        Ok(Some(events))
    }

    /// Post an event, recording the events it announces.
    pub fn post(&self, event: BuildEvent) {
        {
            let mut state = self.lock();
            if let Some(id) = event.id.as_option() {
                state.posted.insert(id_key(id));
            }
            state.announced.extend(event.children.iter().cloned());
        }
        self.log.post(event);
    }

    /// Post a configuration's description, the first time it's needed.
    pub fn configuration(&self, configuration: &events::Configuration<'_>) {
        let first = self.lock().configurations.insert(configuration.id.to_string());
        if first {
            self.post(events::configuration(configuration));
        }
    }

    /// The number of the next named set of files.
    pub fn next_named_set(&self) -> usize {
        let mut state = self.lock();
        let set = state.named_sets;
        state.named_sets += 1;
        set
    }

    /// Record what the command printed, for its progress event.
    pub fn printed(&self, stdout: &str, stderr: &str) {
        let mut state = self.lock();
        state.stdout.push_str(stdout);
        state.stderr.push_str(stderr);
    }

    /// The targets the command's patterns expanded to, with what's said
    /// of them later.
    pub fn patterns_expanded(&self, targets: Vec<(String, TargetAttrs)>) {
        let labels: Vec<String> = targets.iter().map(|(label, _)| label.clone()).collect();
        self.lock().targets.extend(targets);
        if !self.patterns.is_empty() {
            self.post(events::pattern_expanded(&self.patterns, &labels));
        }
    }

    /// Where the command's actions run.
    pub fn workspace_info(&self, exec_root: &Path) {
        self.post(events::workspace_info(exec_root));
    }

    fn target_attrs(&self, label: &str) -> TargetAttrs {
        self.lock().targets.get(label).cloned().unwrap_or_default()
    }

    /// The build's status values.
    pub fn workspace_status(&self, values: &StatusValues) {
        let mut items: Vec<(String, String)> = values.stable.iter().chain(&values.volatile).cloned().collect();
        items.sort();
        self.post(events::workspace_status(&items));
    }

    /// That a top-level target was configured, with its configuration.
    pub fn target_configured(&self, label: &str, target: &ConfiguredTarget) {
        let config = target.config.get();
        self.configuration(&configuration_of(&config));
        let attrs = self.target_attrs(label);
        let size = attrs.test.as_ref().map(|(size, _)| events::test_size(size));
        self.post(events::target_configured(label, &config.digest, target.kind.as_str(), size, &attrs.tags));
    }

    /// That a top-level target finished, with the files it built in named
    /// sets, announcing a test's first attempt at each shard and its
    /// summary.
    pub fn target_completed(&self, label: &str, target: &ConfiguredTarget, built: Option<&[OutputFile]>, exec_root: &Path) {
        let config = target.config.get();
        let root = config.bin_dir();
        let mut groups = Vec::new();
        if let Some(files) = built.filter(|files| !files.is_empty()) {
            let set = self.next_named_set();
            let files = files.iter().map(|file| output_file(file, exec_root, &root)).collect();
            self.post(events::named_set(set, files));
            groups.push(("default".to_string(), vec![set]));
        }
        let attrs = self.target_attrs(label);
        let timeout = attrs.test.as_ref().map(|(_, timeout)| *timeout);
        let mut children = Vec::new();
        if target.is_test && built.is_some() {
            let shards = target.test.as_ref().map_or(1, |test| test.shard_count.max(1));
            children.extend((1..=shards).map(|shard| events::id::test_result(label, &config.digest, 1, shard as i32, 1)));
            children.push(events::id::test_summary(label, &config.digest));
        }
        let mut tags = attrs.tags.clone();
        tags.extend(attrs.implicit_tags.iter().cloned());
        let completed = events::Completed {
            label,
            configuration: &config.digest,
            success: built.is_some(),
            output_groups: groups,
            tags: &tags,
            test_timeout_seconds: timeout,
            children,
        };
        self.post(events::target_completed(completed));
    }

    /// End the stream: what the command printed, any announced events that
    /// won't come, as aborted, and that the command finished with
    /// `exit_code`, with its logs and metrics.
    pub async fn finish(&self, exit_code: i32, actions_executed: i64) -> Result<(), String> {
        let (stdout, stderr) = {
            let mut state = self.lock();
            (std::mem::take(&mut state.stdout), std::mem::take(&mut state.stderr))
        };
        self.post(events::progress(0, &stdout, &stderr, true));
        let finished = [events::id::build_finished(), events::id::build_tool_logs(), events::id::build_metrics()];
        let finished: HashSet<String> = finished.iter().map(id_key).collect();
        for id in self.unposted() {
            if !finished.contains(&id_key(&id)) {
                self.post(events::aborted(id, "The command stopped before this event happened."));
            }
        }
        self.post(events::build_finished(exit_code, SystemTime::now()));
        self.post(events::build_tool_logs(self.started.elapsed().as_secs_f64()));
        let last = events::build_metrics(actions_executed, actions_executed);
        self.log.close(last).await.map_err(|error| format!("Unable to write build events: {error}"))
    }

    /// The events announced but not yet posted, each once.
    fn unposted(&self) -> Vec<BuildEventId> {
        let state = self.lock();
        let mut seen = HashSet::new();
        state
            .announced
            .iter()
            .filter(|id| {
                let key = id_key(id);
                !state.posted.contains(&key) && seen.insert(key)
            })
            .cloned()
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// An event id as a key: its JSON.
fn id_key(id: &BuildEventId) -> String {
    serde_json::to_string(id).unwrap_or_default()
}

/// How the protocol describes a configuration.
pub fn configuration_of(config: &BuildConfiguration) -> events::Configuration<'_> {
    events::Configuration {
        id: &config.digest,
        mnemonic: &config.output_directory,
        platform_name: &config.cpu,
        cpu: &config.cpu,
        make_variables: vec![
            ("BINDIR".to_string(), config.bin_dir()),
            ("COMPILATION_MODE".to_string(), config.compilation_mode.clone()),
            ("GENDIR".to_string(), config.bin_dir()),
            ("TARGET_CPU".to_string(), config.cpu.clone()),
        ],
        is_tool: config.is_exec,
    }
}

/// A file a build made, with its digest and size.
fn output_file(file: &OutputFile, exec_root: &Path, root: &str) -> bazel_bep::events::proto::File {
    let path = file.path.to_string();
    let digest = file.digest.hash.iter().map(|byte| format!("{byte:02x}")).collect();
    events::file(&FileRef {
        exec_path: &path,
        exec_root,
        root,
        digest: Some(digest),
        length: Some(file.digest.size),
    })
}

fn string_attr(rule: &Rule, name: &str) -> Option<String> {
    match rule.attr(Symbol::intern(name)) {
        Some(AttrValue::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

/// A rule's tags.
fn tags(rule: Option<&Rule>) -> Vec<String> {
    match rule.and_then(|rule| rule.attr(Symbol::intern("tags"))) {
        Some(AttrValue::StringList(tags)) => tags.clone(),
        _ => Vec::new(),
    }
}

/// A test's timeout: its `timeout`, or its size's.
fn test_timeout(rule: &Rule) -> &'static str {
    let size = string_attr(rule, "size").unwrap_or_else(|| "medium".to_string());
    let timeout = string_attr(rule, "timeout").unwrap_or_default();
    match (timeout.as_str(), size.as_str()) {
        ("short", _) | ("", "small") => "short",
        ("long", _) | ("", "large") => "long",
        ("eternal", _) | ("", "enormous") => "eternal",
        _ => "moderate",
    }
}

fn timeout_seconds(timeout: &str) -> u64 {
    match timeout {
        "short" => 60,
        "long" => 900,
        "eternal" => 3600,
        _ => 300,
    }
}

/// The tag a boolean attribute adds: its name, or its name after `no`.
fn bool_tag(rule: &Rule, name: &str) -> String {
    match rule.attr(Symbol::intern(name)) {
        Some(AttrValue::Bool(true)) => name.to_string(),
        _ => format!("no{name}"),
    }
}
