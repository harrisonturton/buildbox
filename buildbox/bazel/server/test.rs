//! `bb test`: build targets, then run the tests among them.

use crate::build::{self, Built};
use crate::command::{CommandContext, exit};
use crate::output::Output;
use crate::workspace::Workspace;
use bazel_analysis::ConfiguredTargetKey;
use crate::test_options::TestRepetition;
use bazel_execution::{Backend, CacheResults, TestKey, TestResult, TestRun, TestSettings, TestStatus};
use bazel_loading::PackageKey;
use bazel_options::ParsedOptions;
use bazel_packages::Target;
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap};
use bazel_label::Label;
use skyframe::Session;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// The column test statuses line up at, as Bazel prints them.
const STATUS_COLUMN: usize = 73;

/// The exit code when tests fail.
const TESTS_FAILED: i32 = 3;

/// The exit code when there are no tests to run.
const NO_TESTS_FOUND: i32 = 4;

pub async fn test(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let started = Instant::now();
    let options = match build::parse(context, "test", args, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let settings = match test_settings(context, &options) {
        Ok(settings) => settings,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let report = match SummaryReport::parse(&options) {
        Ok(report) => report,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let repetition = match TestRepetition::parse(&options) {
        Ok(repetition) => repetition,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let epoch = bazel_execution::next_epoch();
    let workspace = context.server.workspaces().get(&context.workspace_root);
    if let Err(message) = crate::command::refresh(context, &workspace, &options).await {
        out.error(&message).await;
        return crate::command::exit::COMMAND_LINE_ERROR;
    }
    if let Err(code) = build::update_status(context, &workspace, &options, out).await {
        return build::failed(code, started, out).await;
    }
    let session = workspace.graph.session().await;
    let built = match build::run_command(context, &session, &options, build::Purpose::Test, out).await {
        Ok(built) => built,
        Err(code) => return build::failed(code, started, out).await,
    };
    build::report(context, &options, &built, out).await;
    if built.errors {
        build::report_stats(started, out).await;
        out.error("Build did NOT complete successfully").await;
        return exit::BUILD_FAILURE;
    }

    let tests = select_tests(&session, &built, &options, &settings, &repetition, &context.env, out).await;
    let skipped: Vec<Label> = built
        .targets
        .iter()
        .filter(|(_, target, _)| target.is_test && target.incompatible.is_some())
        .map(|(label, _, _)| *label)
        .collect();
    if tests.is_empty() && skipped.is_empty() {
        build::report_stats(started, out).await;
        build::completed(0, out).await;
        out.error("No test targets were found, yet testing was requested").await;
        return NO_TESTS_FOUND;
    }
    let session = retry_failures(&workspace, session, &tests, &options).await;
    let output_mode = options.get("test_output").unwrap_or("summary").to_string();
    let keep_going = options.flag("test_keep_going", true);
    let results = run_tests(context, &session, &tests, &built, epoch, &output_mode, keep_going, out).await;
    report_summaries(&results, &built, out).await;
    post_test_summaries(&results, &built, epoch, out);
    drop(session);

    let failed = results.iter().filter(|(_, result)| !passed(result)).count();
    keep_results(&workspace, &results).await;
    build::report_stats(started, out).await;
    build::completed(failed, out).await;
    summarize(&results, &skipped, &built, epoch, &report, out).await;
    match failed {
        0 => exit::SUCCESS,
        _ => TESTS_FAILED,
    }
}

/// How the options say to run tests.
fn test_settings(context: &CommandContext<'_>, options: &ParsedOptions) -> Result<TestSettings, String> {
    Ok(TestSettings {
        filter: options.get("test_filter").map(str::to_string),
        args: options.all("test_arg").into_iter().map(str::to_string).collect(),
        env: test_env(&context.env, &options.all("test_env")),
        timeouts: test_timeouts(options.get("test_timeout").unwrap_or("-1"))?,
        cache_results: match options.get("cache_test_results") {
            Some("true") => CacheResults::Yes,
            Some("false") => CacheResults::No,
            _ => CacheResults::Auto,
        },
        zip_outputs: options.flag("zip_undeclared_test_outputs", false),
    })
}

/// The variables `--test_env`s give: `name=value`, `name` for the client's
/// value if it has one, or `=name` to drop an earlier one.
fn test_env(client: &HashMap<String, String>, given: &[&str]) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for entry in given {
        if let Some(name) = entry.strip_prefix('=') {
            env.remove(name);
            continue;
        }
        let (name, value) = match entry.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (*entry, client.get(*entry).cloned()),
        };
        match value {
            Some(value) => env.insert(name.to_string(), value),
            None => env.remove(name),
        };
    }
    env.into_iter().collect()
}

/// `--test_timeout`'s timeouts for short, moderate, long and eternal
/// tests: one for all, or four, as Bazel parses them, with those that
/// aren't positive left at their defaults.
fn test_timeouts(text: &str) -> Result<Vec<Option<u64>>, String> {
    let mut values = Vec::new();
    for token in text.splitn(6, ',') {
        if token.is_empty() && values.len() <= 1 {
            continue;
        }
        let value: i64 = token.parse().map_err(|_| format!("While parsing option --test_timeout={text}: '{text}' is not an int"))?;
        values.push(u64::try_from(value).ok().filter(|seconds| *seconds > 0));
    }
    match values.len() {
        1 => Ok(vec![values[0]; 4]),
        4 => Ok(values),
        _ => Err(format!("While parsing option --test_timeout={text}: Invalid number of comma-separated entries")),
    }
}

/// The tests to run: those built that are compatible with the
/// configuration and that `--test_tag_filters` selects.
async fn select_tests(
    session: &Session,
    built: &Built,
    options: &ParsedOptions,
    settings: &TestSettings,
    repetition: &TestRepetition,
    client: &HashMap<String, String>,
    out: &Output,
) -> Vec<TestKey> {
    let candidates: Vec<ConfiguredTargetKey> = built
        .targets
        .iter()
        .filter(|(_, target, _)| target.is_test && target.incompatible.is_none())
        .map(|(label, target, _)| ConfiguredTargetKey { label: *label, config: target.config })
        .collect();
    let selected = filter_tests(session, &candidates, built, options).await;
    if selected.is_empty() && !candidates.is_empty() {
        out.warning("All specified test targets were excluded by filters").await;
    }
    let mut keys = Vec::new();
    for target in selected {
        let attrs = built_target(built, target).and_then(|built| built.test).unwrap_or_default();
        let label = target.label.to_string();
        let shards = repetition.shards(attrs.shard_count);
        let attempts = repetition.attempts(&label, attrs.flaky);
        let inherited_env = inherited_env(built, target, client);
        for run in TestRun::all(shards, repetition.runs(&label)) {
            keys.push(TestKey {
                target,
                settings: settings.clone(),
                inherited_env: inherited_env.clone(),
                run,
                attempts,
            });
        }
    }
    keys
}

/// The configured target of a test the build built.
fn built_target(built: &Built, test: ConfiguredTargetKey) -> Option<&bazel_analysis::ConfiguredTarget> {
    let found = built.targets.iter().find(|(label, target, _)| *label == test.label && target.config == test.config);
    found.map(|(_, target, _)| target.as_ref())
}

/// The client's values of the variables a test's `RunEnvironmentInfo`
/// inherits.
fn inherited_env(built: &Built, test: ConfiguredTargetKey, client: &HashMap<String, String>) -> Vec<(String, String)> {
    let Some(target) = built_target(built, test) else {
        return Vec::new();
    };
    let mut env: Vec<(String, String)> = target
        .run_env
        .inherited
        .iter()
        .filter_map(|name| Some((name.clone(), client.get(name)?.clone())))
        .collect();
    env.sort();
    env.dedup();
    env
}

/// Let the tests that failed before run again, unless
/// `--cache_test_results=yes` reuses their failures, returning the session
/// to run them in.
async fn retry_failures(workspace: &Workspace, session: Session, tests: &[TestKey], options: &ParsedOptions) -> Session {
    if options.get("cache_test_results") == Some("true") {
        return session;
    }
    let stale = workspace.failed_tests.take_among(tests);
    if stale.is_empty() {
        return session;
    }
    drop(session);
    workspace.graph.invalidate(&stale).await;
    workspace.graph.session().await
}

/// Keep the results later commands may reuse: passes, and failures, which
/// only `--cache_test_results=yes` reuses.
async fn keep_results(workspace: &Workspace, results: &[(TestKey, Result<Arc<TestResult>, String>)]) {
    let unreusable: Vec<TestKey> = results.iter().filter(|(_, result)| !reusable(result)).map(|(key, _)| key.clone()).collect();
    workspace.graph.invalidate(&unreusable).await;
    let failures = results.iter().filter(|(_, result)| reusable(result) && !passed(result));
    workspace.failed_tests.extend(failures.map(|(key, _)| key.clone()));
}

/// The tests among `candidates` that the test filters select.
async fn filter_tests(session: &Session, candidates: &[ConfiguredTargetKey], built: &Built, options: &ParsedOptions) -> Vec<ConfiguredTargetKey> {
    let filter = build::test_filter(options);
    let mut selected = Vec::new();
    for candidate in candidates {
        let Ok(package) = session.evaluate(&PackageKey(candidate.label.package)).await else {
            continue;
        };
        let rule = package.0.target(candidate.label.name).and_then(Target::as_rule);
        let kind = built_target(built, *candidate).map(|target| target.kind.as_str()).unwrap_or_default();
        if filter.passes(rule, kind) {
            selected.push(*candidate);
        }
    }
    selected
}

/// Whether a later command may reuse a test's result: one that can be
/// cached, though failures only with `--cache_test_results=yes`.
fn reusable(result: &Result<Arc<TestResult>, String>) -> bool {
    matches!(result, Ok(test) if test.reusable)
}

/// The width to pad a label to, so that statuses end in the same column.
fn status_width(label: &str, prefix: &str, word: &str) -> usize {
    (STATUS_COLUMN + "PASSED".len()).saturating_sub(prefix.len() + word.len()).max(label.len() + 1)
}

fn passed(result: &Result<Arc<TestResult>, String>) -> bool {
    matches!(result, Ok(result) if matches!(result.status, TestStatus::Passed | TestStatus::Flaky))
}

/// Run the tests, writing their logs to the testlogs tree and reporting
/// failures as they're found.
async fn run_tests(
    context: &CommandContext<'_>,
    session: &Session,
    tests: &[TestKey],
    built: &Built,
    epoch: u64,
    output_mode: &str,
    keep_going: bool,
    out: &Output,
) -> Vec<(TestKey, Result<Arc<TestResult>, String>)> {
    let results = match keep_going {
        true => session.evaluate_many(tests).await,
        false => until_failure(session, tests).await,
    };
    let mut reported = Vec::with_capacity(tests.len());
    for (key, result) in tests.iter().zip(results) {
        let result = result.map_err(|error| error.message().to_string());
        if let Ok(test) = &result {
            write_logs(context.server.backend(), built, key, test).await;
            report_test(key, test, built, epoch, output_mode, out).await;
            post_test_results(key, test, built, epoch, out);
        }
        match &result {
            Err(message) if !message.is_empty() => out.error(message).await,
            _ => {}
        }
        reported.push((key.clone(), result));
    }
    if !keep_going {
        report_aborted(session, &context.workspace_root, &reported, built, out).await;
    }
    reported
}

/// Run tests until one fails, as `--notest_keep_going` asks: those still
/// running then are cancelled and have no status, as those not yet run.
async fn until_failure(session: &Session, tests: &[TestKey]) -> Vec<Result<Arc<TestResult>, skyframe::SkyError>> {
    let mut results: Vec<Option<Result<Arc<TestResult>, skyframe::SkyError>>> = tests.iter().map(|_| None).collect();
    let mut running: futures::stream::FuturesUnordered<_> = tests.iter().enumerate().map(|(index, test)| async move { (index, session.evaluate(test).await) }).collect();
    while let Some((index, result)) = running.next().await {
        let failed = !matches!(&result, Ok(result) if matches!(result.status, TestStatus::Passed | TestStatus::Flaky));
        results[index] = Some(result);
        if failed {
            break;
        }
    }
    drop(running);
    results.into_iter().map(|result| result.unwrap_or_else(|| Err(skyframe::SkyError::new("")))).collect()
}

/// Report that a test failed and `--notest_keep_going` stopped the rest,
/// as Bazel does.
async fn report_aborted(session: &Session, workspace: &std::path::Path, results: &[(TestKey, Result<Arc<TestResult>, String>)], built: &Built, out: &Output) {
    let failed = results.iter().find(|(_, result)| matches!(result, Ok(result) if !matches!(result.status, TestStatus::Passed | TestStatus::Flaky)));
    let Some((key, _)) = failed else {
        return;
    };
    let failed = format!("Testing {} failed: Test failed, aborting", test_name(key, built));
    let message = match bazel_loading::target_location_in(session, workspace, key.target.label).await {
        Some(location) => format!("{location}: {failed}"),
        None => failed,
    };
    out.error(&message).await;
    out.error("Test failed, aborting").await;
}

/// How `--test_summary` and `--test_verbose_timeout_warnings` say to
/// report tests' results.
struct SummaryReport {
    /// Whether to list every test, rather than only those that didn't
    /// pass.
    all_tests: bool,
    /// Whether to list tests that passed without running.
    cached_tests: bool,
    /// Whether to list tests at all.
    listed: bool,
    verbose_timeouts: bool,
}

impl SummaryReport {
    fn parse(options: &ParsedOptions) -> Result<SummaryReport, String> {
        let verbose_timeouts = options.flag("test_verbose_timeout_warnings", false);
        let format = options.get("test_summary").unwrap_or("short");
        let (listed, all_tests, cached_tests) = match format {
            "short" => (true, true, true),
            "short_uncached" => (true, true, false),
            "terse" => (true, false, false),
            "none" => (false, false, false),
            "detailed" | "detailed_uncached" | "testcase" => {
                return Err(format!("--test_summary={format} isn't supported by bb yet: it lists test cases"));
            }
            _ => return Err(format!("While parsing option --test_summary={format}: Not a valid test summary format: '{format}'")),
        };
        match options.get("test_output").unwrap_or("summary") {
            "summary" | "errors" | "all" | "streamed" => {}
            other => return Err(format!("While parsing option --test_output={other}: Not a valid test output format: '{other}'")),
        }
        Ok(SummaryReport { all_tests, cached_tests, listed, verbose_timeouts })
    }
}

/// Where a test's logs go.
fn testlogs_dir(built: &Built, key: &TestKey) -> PathBuf {
    let config = built.config.get();
    let relative = bazel_analysis::package_dir(key.target.label.package).join_path(bazel_analysis::name_path(key.target.label));
    let dir = built.exec_root.join("bazel-out").join(&config.output_directory).join("testlogs").join(relative.to_string());
    match key.run.dir() {
        Some(run) => dir.join(run),
        None => dir,
    }
}

/// The logs of the failed attempts before a test's last.
fn attempt_log(built: &Built, key: &TestKey, attempt: usize, extension: &str) -> PathBuf {
    testlogs_dir(built, key).join("test_attempts").join(format!("attempt_{}.{extension}", attempt + 1))
}

/// The logs of a test's run that failed: its failed attempts', and its
/// own if it failed too.
fn failed_logs(built: &Built, key: &TestKey, result: &TestResult) -> Vec<PathBuf> {
    let mut logs: Vec<PathBuf> = (0..result.failed_attempts.len()).map(|attempt| attempt_log(built, key, attempt, "log")).collect();
    if !matches!(result.status, TestStatus::Passed | TestStatus::Flaky) {
        logs.push(testlogs_dir(built, key).join("test.log"));
    }
    logs
}

/// Write a test's `test.log`, `test.xml`, the other files it left, and
/// the logs of its failed attempts.
async fn write_logs(backend: Option<&Arc<Backend>>, built: &Built, key: &TestKey, result: &TestResult) {
    let dir = testlogs_dir(built, key);
    let Some(backend) = backend else {
        return;
    };
    for stale in ["test_attempts", "test.outputs", "test.outputs_manifest"] {
        let _ = tokio::fs::remove_dir_all(dir.join(stale)).await;
    }
    let mut files: Vec<(PathBuf, Option<bazel_execution::BlobDigest>)> = vec![(dir.join("test.log"), result.log), (dir.join("test.xml"), result.xml)];
    files.extend(result.outputs.iter().map(|(path, digest)| (dir.join(path), Some(*digest))));
    for (index, attempt) in result.failed_attempts.iter().enumerate() {
        files.push((attempt_log(built, key, index, "log"), attempt.log));
        files.push((attempt_log(built, key, index, "xml"), attempt.xml));
    }
    if let Some(tree) = result.undeclared_outputs {
        let dest = dir.join(bazel_execution::UNDECLARED_OUTPUTS);
        if let Err(error) = bazel_execution::write_tree(backend, &tree.to_cas(), &dest).await {
            tracing::warn!("failed to write {}: {}", dest.display(), error.message());
        }
    }
    for (path, digest) in files {
        let _ = tokio::fs::remove_file(&path).await;
        let Some(digest) = digest else {
            continue;
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(error) = backend.cas.materialize(&digest.to_cas(), &path, false).await {
            tracing::warn!("failed to write {}: {error}", path.display());
        }
    }
}

/// Post the build events of each attempt at a test's run, if the command
/// writes them.
fn post_test_results(key: &TestKey, result: &TestResult, built: &Built, epoch: u64, out: &Output) {
    let Some(events) = out.build_events() else {
        return;
    };
    let label = bazel_query::display_label(key.target.label, &built.main_mapping);
    let config = key.target.config.get();
    let cached = result.cached || result.epoch < epoch;
    let attempt = |number: usize, status: TestStatus, exit_code: i32, duration_ms: u64, outputs: Vec<bazel_bep::events::proto::File>| bazel_bep::events::TestAttempt {
        label: &label,
        configuration: &config.digest,
        run: key.run.run as i32 + 1,
        shard: key.run.shard as i32 + 1,
        attempt: number as i32,
        status: event_status(status),
        cached,
        start: std::time::SystemTime::now() - std::time::Duration::from_millis(duration_ms),
        duration_ms,
        exit_code,
        outputs,
        strategy: "linux-sandbox",
    };
    for (index, failed) in result.failed_attempts.iter().enumerate() {
        let outputs = attempt_outputs(&attempt_log(built, key, index, "log"), failed.xml.map(|_| attempt_log(built, key, index, "xml")));
        events.post(bazel_bep::events::test_result(attempt(index + 1, failed.status, failed.exit_code, failed.duration_ms, outputs)));
    }
    let dir = testlogs_dir(built, key);
    let outputs = attempt_outputs(&dir.join("test.log"), result.xml.map(|_| dir.join("test.xml")));
    let last = result.failed_attempts.len() + 1;
    events.post(bazel_bep::events::test_result(attempt(last, result.status, result.exit_code, result.duration_ms, outputs)));
}

/// The files an attempt at a test leaves, as build events name them.
fn attempt_outputs(log: &std::path::Path, xml: Option<PathBuf>) -> Vec<bazel_bep::events::proto::File> {
    let mut outputs = vec![bazel_bep::events::file_at("test.log", log)];
    if let Some(xml) = xml {
        outputs.push(bazel_bep::events::file_at("test.xml", &xml));
    }
    outputs
}

fn event_status(status: TestStatus) -> bazel_bep::events::TestStatus {
    match status {
        TestStatus::Passed => bazel_bep::events::TestStatus::Passed,
        TestStatus::Flaky => bazel_bep::events::TestStatus::Flaky,
        TestStatus::Failed => bazel_bep::events::TestStatus::Failed,
        TestStatus::TimedOut => bazel_bep::events::TestStatus::TimedOut,
    }
}

/// Post the build event summarizing each test's runs, if the command
/// writes them.
fn post_test_summaries(results: &[(TestKey, Result<Arc<TestResult>, String>)], built: &Built, epoch: u64, out: &Output) {
    let Some(events) = out.build_events() else {
        return;
    };
    for summary in summaries(results) {
        let first = summary.runs[0].0;
        let label = bazel_query::display_label(summary.label, &built.main_mapping);
        let config = first.target.config.get();
        let mut passed = Vec::new();
        let mut failed = Vec::new();
        for (key, result) in &summary.runs {
            let Ok(result) = result else {
                continue;
            };
            let log = bazel_bep::events::file_at("", &testlogs_dir(built, key).join("test.log"));
            match result.status {
                TestStatus::Passed | TestStatus::Flaky => passed.push(log),
                TestStatus::Failed | TestStatus::TimedOut => failed.push(log),
            }
        }
        let total_ms: u64 = summary.times_ms().iter().sum();
        let now = std::time::SystemTime::now();
        events.post(bazel_bep::events::test_summary(bazel_bep::events::Summary {
            label: &label,
            configuration: &config.digest,
            status: summary.status().map(event_status).unwrap_or(bazel_bep::events::TestStatus::NoStatus),
            total_runs: summary.runs.len() as i32,
            runs: first.run.runs.max(1) as i32,
            shards: first.run.shards as i32,
            attempts: summary.attempts() as i32,
            cached: summary.cached(epoch) as i32,
            passed,
            failed,
            first_start: now - std::time::Duration::from_millis(total_ms),
            last_stop: now,
            total_duration_ms: total_ms,
        }));
    }
}

/// The test's label, and which shard and run of it `key` is.
fn test_name(key: &TestKey, built: &Built) -> String {
    let shown = bazel_query::display_label(key.target.label, &built.main_mapping);
    match key.run.suffix().as_str() {
        "" => shown,
        suffix => format!("{shown} {suffix}"),
    }
}

async fn report_test(key: &TestKey, result: &TestResult, built: &Built, epoch: u64, output_mode: &str, out: &Output) {
    let shown = test_name(key, built);
    let log = testlogs_dir(built, key).join("test.log");
    if result.epoch >= epoch {
        for (index, attempt) in result.failed_attempts.iter().enumerate() {
            let reason = failure_reason(attempt.status, attempt.exit_code);
            out.stderr(&format!("FAIL: {shown} ({reason}) (see {})\n", attempt_log(built, key, index, "log").display())).await;
        }
    }
    let failed = !matches!(result.status, TestStatus::Passed | TestStatus::Flaky);
    if failed && result.epoch >= epoch {
        let reason = failure_reason(result.status, result.exit_code);
        out.stderr(&format!("FAIL: {shown} ({reason}) (see {})\n", log.display())).await;
    }
    let show_log = matches!(output_mode, "all" | "streamed") || (output_mode == "errors" && failed);
    if show_log && (result.epoch >= epoch || failed) {
        let text = tokio::fs::read_to_string(&log).await.unwrap_or_default();
        out.info(&format!("From Testing {shown}:")).await;
        out.stdout(&format!("==================== Test output for {shown}:\n{}{}\n", without_preamble(&text), "=".repeat(80))).await;
    }
}

/// Why a run failed, as Bazel says it.
fn failure_reason(status: TestStatus, exit_code: i32) -> String {
    match status {
        TestStatus::TimedOut => "TIMEOUT".to_string(),
        _ => format!("Exit {exit_code}"),
    }
}

/// A test log without what test-setup.sh prints before the test runs: up
/// to its line of dashes, as Bazel shows logs.
fn without_preamble(log: &str) -> &str {
    const SEPARATOR: &str = "-----------------------------------------------------------------------------\n";
    match log.find(SEPARATOR) {
        Some(index) => &log[index + SEPARATOR.len()..],
        None => log,
    }
}

/// The results of a test's shards and runs, together, as Bazel summarizes
/// them.
struct Summary<'a> {
    label: Label,
    /// Each run's key and result.
    runs: Vec<(&'a TestKey, &'a Result<Arc<TestResult>, String>)>,
}

impl Summary<'_> {
    /// The worst of the runs' statuses, as Bazel aggregates them, or
    /// `None` if a run has none.
    fn status(&self) -> Option<TestStatus> {
        let mut worst = TestStatus::Passed;
        for (_, result) in &self.runs {
            let Ok(result) = result else {
                return None;
            };
            if severity(result.status) > severity(worst) {
                worst = result.status;
            }
        }
        Some(worst)
    }

    /// How many runs' results were cached, by the action cache or from an
    /// earlier command.
    fn cached(&self, epoch: u64) -> usize {
        let cached = |result: &TestResult| result.cached || result.epoch < epoch;
        self.runs.iter().filter(|(_, result)| matches!(result, Ok(result) if cached(result))).count()
    }

    fn failed_logs(&self, built: &Built) -> Vec<PathBuf> {
        let mut logs = Vec::new();
        for (key, result) in &self.runs {
            if let Ok(result) = result {
                logs.extend(failed_logs(built, key, result));
            }
        }
        logs
    }

    /// How many attempts ran, passed or failed.
    fn attempts(&self) -> usize {
        let each = |result: &Result<Arc<TestResult>, String>| match result {
            Ok(result) => result.failed_attempts.len() + 1,
            Err(_) => 0,
        };
        self.runs.iter().map(|(_, result)| each(result)).sum()
    }

    /// How long each attempt of each run took.
    fn times_ms(&self) -> Vec<u64> {
        let mut times = Vec::new();
        for (_, result) in &self.runs {
            if let Ok(result) = result {
                times.extend(result.failed_attempts.iter().map(|attempt| attempt.duration_ms));
                times.push(result.duration_ms);
            }
        }
        times
    }

    /// Bazel's warning that the test's longest run took a time outside
    /// the range its timeout is meant for, suggesting a better one.
    fn size_warning(&self, label: &str) -> Option<String> {
        let timeout = self.runs.iter().find_map(|(_, result)| result.as_ref().ok().map(|result| result.timeout_seconds))?;
        let max_ms = self.times_ms().into_iter().max()?;
        size_warning(label, timeout, max_ms)
    }

    /// How many attempts ran in this command rather than coming from a
    /// cache.
    fn uncached_attempts(&self, epoch: u64) -> usize {
        let each = |result: &TestResult| match result.cached || result.epoch < epoch {
            true => 0,
            false => result.failed_attempts.len() + 1,
        };
        self.runs.iter().filter_map(|(_, result)| result.as_ref().ok()).map(|result| each(result)).sum()
    }
}

/// The timeout categories: their names, timeouts in seconds, and the
/// sizes they're the default of.
const TIMEOUT_CATEGORIES: [(&str, u64, &str); 4] = [("short", 60, "small"), ("moderate", 300, "medium"), ("long", 900, "large"), ("eternal", 3600, "enormous")];

/// The longest time Bazel considers, a year.
const YEAR_SECONDS: u64 = 365 * 24 * 60 * 60;

/// Bazel's warning that a test whose timeout is `timeout` seconds took
/// `max_ms` at most, outside of half the next shorter timeout and 90% of
/// its own, with the timeout it suggests: the first whose 75% it fits in.
fn size_warning(label: &str, timeout: u64, max_ms: u64) -> Option<String> {
    let seconds = max_ms / 1000;
    let index = TIMEOUT_CATEGORIES.iter().position(|(_, seconds, _)| *seconds == timeout).unwrap_or(1);
    let last = TIMEOUT_CATEGORIES.len() - 1;
    let min = match index {
        0 => 0,
        _ => TIMEOUT_CATEGORIES[index - 1].1 / 2,
    };
    let max = match index == last {
        true => YEAR_SECONDS,
        false => TIMEOUT_CATEGORIES[index].1 * 9 / 10,
    };
    if (min..max).contains(&seconds) {
        return None;
    }
    let suggested = TIMEOUT_CATEGORIES.iter().enumerate().find(|(position, (_, timeout, _))| *position == last || seconds < timeout * 3 / 4);
    let (_, (name, _, size)) = suggested?;
    Some(format!(
        "{label}: Test execution time ({:.1}s excluding execution overhead) outside of range for {} tests. Consider setting timeout=\"{name}\" or size=\"{size}\".",
        max_ms as f64 / 1000.0,
        TIMEOUT_CATEGORIES[index].0.to_uppercase()
    ))
}

/// How bad a status is, as Bazel orders `BlazeTestStatus`.
fn severity(status: TestStatus) -> u8 {
    match status {
        TestStatus::Passed => 1,
        TestStatus::Flaky => 2,
        TestStatus::TimedOut => 3,
        TestStatus::Failed => 4,
    }
}

fn status_word(status: Option<TestStatus>) -> &'static str {
    match status {
        Some(TestStatus::Passed) => "PASSED",
        Some(TestStatus::Flaky) => "FLAKY",
        Some(TestStatus::Failed) => "FAILED",
        Some(TestStatus::TimedOut) => "TIMEOUT",
        None => "NO STATUS",
    }
}

/// The results of each test, its shards' and runs' together, in the order
/// they were run.
fn summaries<'a>(results: &'a [(TestKey, Result<Arc<TestResult>, String>)]) -> Vec<Summary<'a>> {
    let mut summaries: Vec<Summary<'a>> = Vec::new();
    for (key, result) in results {
        match summaries.iter_mut().find(|summary| summary.label == key.target.label && summary.runs[0].0.target == key.target) {
            Some(summary) => summary.runs.push((key, result)),
            None => summaries.push(Summary {
                label: key.target.label,
                runs: vec![(key, result)],
            }),
        }
    }
    summaries
}

/// Report what Bazel reports when a test's results are in: the logs of
/// each failure, for statuses that say more than a single failure does.
async fn report_summaries(results: &[(TestKey, Result<Arc<TestResult>, String>)], built: &Built, out: &Output) {
    for summary in summaries(results) {
        let status = summary.status();
        let logs = summary.failed_logs(built);
        let news = match status {
            None | Some(TestStatus::Passed) => false,
            Some(TestStatus::Failed) => logs.len() != 1,
            Some(_) => true,
        };
        if !news {
            continue;
        }
        let label = bazel_query::display_label(summary.label, &built.main_mapping);
        let mut text = format!("{}: {label} (Summary)\n", status_word(status));
        for log in logs {
            text.push_str(&format!("      {}\n", log.display()));
        }
        out.stderr(&text).await;
    }
}

/// Print each test's status and the totals, as Bazel does: skipped tests,
/// then passes, then failures.
async fn summarize(results: &[(TestKey, Result<Arc<TestResult>, String>)], skipped: &[Label], built: &Built, epoch: u64, report: &SummaryReport, out: &Output) {
    for label in skipped.iter().filter(|_| report.listed && report.all_tests) {
        let label = bazel_query::display_label(*label, &built.main_mapping);
        out.stdout(&format!("{label:<width$}SKIPPED\n", width = status_width(&label, "", "SKIPPED"))).await;
    }
    let mut executed = 0;
    let mut passes = 0;
    let mut failures = 0;
    let mut no_status = 0;
    let mut wrong_size = false;
    let mut summaries = summaries(results);
    summaries.sort_by_key(|summary| summary_order(summary, epoch));
    for summary in &summaries {
        let status = summary.status();
        let cached = summary.cached(epoch);
        executed += usize::from(status.is_some() && cached < summary.runs.len());
        let label = bazel_query::display_label(summary.label, &built.main_mapping);
        let size_warning = summary.size_warning(&summary.label.to_string()).filter(|_| status == Some(TestStatus::Passed));
        wrong_size |= size_warning.is_some() && !report.verbose_timeouts;
        match status {
            Some(TestStatus::Passed | TestStatus::Flaky) => passes += 1,
            Some(_) => failures += 1,
            None => no_status += 1,
        }
        let passed = status == Some(TestStatus::Passed);
        if !report.listed || (passed && !report.all_tests) || (passed && cached == summary.runs.len() && !report.cached_tests) {
            continue;
        }
        let word = status_word(status);
        let prefix = match (status, cached) {
            (None, _) | (_, 0) => String::new(),
            (_, cached) if cached == summary.runs.len() => "(cached) ".to_string(),
            (_, cached) => format!("({cached}/{} cached) ", summary.runs.len()),
        };
        let width = status_width(&label, &prefix, word);
        let details = match status {
            None => String::new(),
            Some(status) => attempts_detail(status, summary.failed_logs(built).len(), summary.attempts()) + &time_detail(&summary.times_ms()),
        };
        out.stdout(&format!("{label:<width$}{prefix}{word}{details}\n")).await;
        if let Some(warning) = size_warning.filter(|_| report.verbose_timeouts) {
            out.stdout(&format!("  WARNING: {warning}\n")).await;
        }
        for log in summary.failed_logs(built) {
            out.stdout(&format!("  {}\n", log.display())).await;
        }
    }
    let total = summaries.len() + skipped.len();
    let tests = if total == 1 { "test" } else { "tests" };
    let mut parts = Vec::new();
    match passes {
        0 => {}
        1 => parts.push("1 test passes".to_string()),
        count => parts.push(format!("{count} tests pass")),
    }
    match failures {
        0 => {}
        1 => parts.push("1 fails locally".to_string()),
        count => parts.push(format!("{count} fail locally")),
    }
    match skipped.len() + no_status {
        0 => {}
        1 => parts.push("1 was skipped".to_string()),
        count => parts.push(format!("{count} were skipped")),
    }
    out.stdout(&format!("\nExecuted {executed} out of {total} {tests}: {}.\n", english_list(&parts))).await;
    if wrong_size {
        out.stdout("There were tests whose specified size is too big. Use the --test_verbose_timeout_warnings command line option to see which ones these are.\n").await;
    }
}

/// How many of a test's attempts failed, as Bazel adds it to the status
/// of a test tried more than once.
fn attempts_detail(status: TestStatus, failed: usize, attempts: usize) -> String {
    if attempts <= 1 {
        return String::new();
    }
    match status {
        TestStatus::Flaky => format!(", failed in {failed} out of {attempts}"),
        TestStatus::Failed | TestStatus::TimedOut => format!(" in {failed} out of {attempts}"),
        TestStatus::Passed => String::new(),
    }
}

/// How long a test took: its one run's time, or for several, the longest
/// and statistics over them, as Bazel prints them.
fn time_detail(times_ms: &[u64]) -> String {
    let seconds = |ms: u64| format!("{:.1}s", ms as f64 / 1000.0);
    match times_ms {
        [] => String::new(),
        [only] => format!(" in {}", seconds(*only)),
        times => {
            let max = times.iter().copied().max().unwrap_or(0);
            let min = times.iter().copied().min().unwrap_or(0);
            let sum: u64 = times.iter().sum();
            let count = times.len() as f64;
            let mean = sum as f64 / count;
            let squares: f64 = times.iter().map(|time| (*time as f64) * (*time as f64)).sum();
            let deviation = ((squares - sum as f64 * mean) / count).max(0.0).sqrt();
            format!(
                " in {}\n  Stats over {} runs: max = {}, min = {}, avg = {}, dev = {}",
                seconds(max),
                times.len(),
                seconds(max),
                seconds(min),
                seconds(mean as u64),
                seconds(deviation as u64)
            )
        }
    }
}

/// Where Bazel lists a test's result: cached ones first, then those with
/// fewer attempts run, then by status, passes first, then by label.
fn summary_order(summary: &Summary<'_>, epoch: u64) -> (bool, usize, i32, String) {
    let status = match summary.status() {
        Some(TestStatus::Passed) => -1,
        Some(TestStatus::Flaky) => 2,
        Some(TestStatus::TimedOut) => 3,
        Some(TestStatus::Failed) => 4,
        None => 0,
    };
    let cached = summary.status().is_some() && summary.cached(epoch) == summary.runs.len();
    (!cached, summary.uncached_attempts(epoch), status, summary.label.to_string())
}

/// Items joined as Bazel joins them in English: `a`, `a and b`, or `a, b,
/// and c`.
fn english_list(items: &[String]) -> String {
    match items {
        [] => "nothing".to_string(),
        [only] => only.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeouts_parse_as_bazel_parses_them() {
        assert_eq!(test_timeouts("-1"), Ok(vec![None; 4]));
        assert_eq!(test_timeouts("7"), Ok(vec![Some(7); 4]));
        assert_eq!(test_timeouts("7,"), Ok(vec![Some(7); 4]));
        assert_eq!(test_timeouts("1,-1,0,4"), Ok(vec![Some(1), None, None, Some(4)]));
        assert!(test_timeouts("1,2").unwrap_err().ends_with("Invalid number of comma-separated entries"));
        assert!(test_timeouts("x").unwrap_err().ends_with("'x' is not an int"));
    }

    #[test]
    fn test_env_inherits_and_drops_variables() {
        let client = HashMap::from([("HOME".to_string(), "/home/me".to_string())]);
        let env = test_env(&client, &["A=1", "HOME", "UNSET", "B=2", "=B"]);
        assert_eq!(env, [("A".to_string(), "1".to_string()), ("HOME".to_string(), "/home/me".to_string())]);
    }
}
