//! `bb test`: build targets, then run the tests among them.

use crate::build::{self, Built};
use crate::command::{CommandContext, exit};
use crate::output::Output;
use crate::workspace::Workspace;
use bazel_analysis::ConfiguredTargetKey;
use bazel_execution::{Backend, TestKey, TestResult, TestSettings, TestStatus};
use bazel_loading::PackageKey;
use bazel_options::ParsedOptions;
use bazel_packages::Target;
use std::collections::{BTreeMap, HashMap};
use bazel_label::Label;
use skyframe::Session;
use std::path::{Path, PathBuf};
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
    let epoch = bazel_execution::next_epoch();
    let workspace = context.server.workspaces().get(&context.workspace_root);
    crate::command::refresh(context, &workspace, &options).await;
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

    let tests = select_tests(&session, &built, &options, &settings, out).await;
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
    let results = run_tests(context, &session, &tests, &built, epoch, &output_mode, out).await;
    drop(session);

    let failed = results.iter().filter(|(_, result)| !passed(result)).count();
    keep_results(&workspace, &results).await;
    build::report_stats(started, out).await;
    build::completed(failed, out).await;
    summarize(&results, &skipped, &built, epoch, out).await;
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
        cached: options.get("cache_test_results") != Some("false"),
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
async fn select_tests(session: &Session, built: &Built, options: &ParsedOptions, settings: &TestSettings, out: &Output) -> Vec<TestKey> {
    let candidates: Vec<ConfiguredTargetKey> = built
        .targets
        .iter()
        .filter(|(_, target, _)| target.is_test && target.incompatible.is_none())
        .map(|(label, target, _)| ConfiguredTargetKey { label: *label, config: target.config })
        .collect();
    let selected = filter_tests(session, &candidates, options).await;
    if selected.is_empty() && !candidates.is_empty() {
        out.warning("All specified test targets were excluded by filters").await;
    }
    selected.into_iter().map(|target| TestKey { target, settings: settings.clone() }).collect()
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

/// The tests among `candidates` that `--test_tag_filters` selects.
async fn filter_tests(session: &Session, candidates: &[ConfiguredTargetKey], options: &ParsedOptions) -> Vec<ConfiguredTargetKey> {
    if options.get("test_tag_filters").unwrap_or_default().is_empty() {
        return candidates.to_vec();
    }
    let mut selected = Vec::new();
    for candidate in candidates {
        let Ok(package) = session.evaluate(&PackageKey(candidate.label.package)).await else {
            continue;
        };
        let rule = package.0.target(candidate.label.name).and_then(Target::as_rule);
        if build::passes_test_filters(rule, options) {
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
    matches!(result, Ok(result) if result.status == TestStatus::Passed)
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
    out: &Output,
) -> Vec<(TestKey, Result<Arc<TestResult>, String>)> {
    let results = session.evaluate_many(tests).await;
    let mut reported = Vec::with_capacity(tests.len());
    for (key, result) in tests.iter().zip(results) {
        let result = result.map_err(|error| error.message().to_string());
        if let Ok(test) = &result {
            let log = write_logs(context.server.backend(), built, key, test).await;
            report_test(key, test, &log, built, epoch, output_mode, out).await;
        }
        if let Err(message) = &result {
            out.error(message).await;
        }
        reported.push((key.clone(), result));
    }
    reported
}

/// Where a test's logs go.
fn testlogs_dir(built: &Built, key: &TestKey) -> PathBuf {
    let config = built.config.get();
    let relative = bazel_analysis::package_dir(key.target.label.package).join_path(bazel_analysis::name_path(key.target.label));
    built.exec_root.join("bazel-out").join(&config.output_directory).join("testlogs").join(relative.to_string())
}

/// Write a test's `test.log` and `test.xml`, returning the log's path.
async fn write_logs(backend: Option<&Arc<Backend>>, built: &Built, key: &TestKey, result: &TestResult) -> PathBuf {
    let dir = testlogs_dir(built, key);
    let log = dir.join("test.log");
    let Some(backend) = backend else {
        return log;
    };
    let _ = tokio::fs::create_dir_all(&dir).await;
    for (name, digest) in [("test.log", result.log), ("test.xml", result.xml)] {
        let path = dir.join(name);
        let _ = tokio::fs::remove_file(&path).await;
        let Some(digest) = digest else {
            continue;
        };
        if let Err(error) = backend.cas.materialize(&digest.to_cas(), &path, false).await {
            tracing::warn!("failed to write {}: {error}", path.display());
        }
    }
    log
}

async fn report_test(key: &TestKey, result: &TestResult, log: &Path, built: &Built, epoch: u64, output_mode: &str, out: &Output) {
    let shown = bazel_query::display_label(key.target.label, &built.main_mapping);
    let failed = result.status != TestStatus::Passed;
    if failed && result.epoch >= epoch {
        let reason = match result.status {
            TestStatus::TimedOut => "TIMEOUT".to_string(),
            _ => format!("Exit {}", result.exit_code),
        };
        out.stderr(&format!("FAIL: {shown} ({reason}) (see {})\n", log.display())).await;
    }
    let show_log = output_mode == "all" || (output_mode == "errors" && failed);
    if show_log && (result.epoch >= epoch || failed) {
        let text = tokio::fs::read_to_string(log).await.unwrap_or_default();
        out.info(&format!("From Testing {shown}:")).await;
        out.stdout(&format!("==================== Test output for {shown}:\n{}{}\n", without_preamble(&text), "=".repeat(80))).await;
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

/// Print each test's status and the totals, as Bazel does: skipped tests,
/// then passes, then failures.
async fn summarize(results: &[(TestKey, Result<Arc<TestResult>, String>)], skipped: &[Label], built: &Built, epoch: u64, out: &Output) {
    for label in skipped {
        let label = bazel_query::display_label(*label, &built.main_mapping);
        out.stdout(&format!("{label:<width$}SKIPPED\n", width = status_width(&label, "", "SKIPPED"))).await;
    }
    let mut executed = 0;
    let mut passes = 0;
    let mut failures = 0;
    let mut wrong_size = false;
    let mut ordered: Vec<&(TestKey, Result<Arc<TestResult>, String>)> = results.iter().collect();
    ordered.sort_by_key(|(key, result)| summary_order(result, epoch, key));
    for (key, result) in ordered {
        let (word, prefix, seconds) = match result {
            Ok(result) => {
                let cached = result.cached || result.epoch < epoch;
                executed += usize::from(!cached);
                wrong_size |= result.status == TestStatus::Passed && result.wrong_size();
                let word = match result.status {
                    TestStatus::Passed => "PASSED",
                    TestStatus::Failed => "FAILED",
                    TestStatus::TimedOut => "TIMEOUT",
                };
                (word, if cached { "(cached) " } else { "" }, result.duration_ms as f64 / 1000.0)
            }
            Err(_) => ("NO STATUS", "", 0.0),
        };
        match word {
            "PASSED" => passes += 1,
            _ => failures += 1,
        }
        let label = bazel_query::display_label(key.target.label, &built.main_mapping);
        let width = status_width(&label, prefix, word);
        out.stdout(&format!("{label:<width$}{prefix}{word} in {seconds:.1}s\n")).await;
        if word != "PASSED" {
            out.stdout(&format!("  {}\n", testlogs_dir(built, key).join("test.log").display())).await;
        }
    }
    let total = results.len() + skipped.len();
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
    match skipped.len() {
        0 => {}
        1 => parts.push("1 was skipped".to_string()),
        count => parts.push(format!("{count} were skipped")),
    }
    out.stdout(&format!("\nExecuted {executed} out of {total} {tests}: {}.\n", english_list(&parts))).await;
    if wrong_size {
        out.stdout("There were tests whose specified size is too big. Use the --test_verbose_timeout_warnings command line option to see which ones these are.\n").await;
    }
}

/// Where Bazel lists a test's result: cached ones first, then by status,
/// passes first, then by label.
fn summary_order(result: &Result<Arc<TestResult>, String>, epoch: u64, key: &TestKey) -> (bool, i32, String, TestKey) {
    let (cached, status) = match result {
        Ok(result) => {
            let status = match result.status {
                TestStatus::Passed => -1,
                TestStatus::TimedOut => 3,
                TestStatus::Failed => 4,
            };
            (result.cached || result.epoch < epoch, status)
        }
        Err(_) => (false, 0),
    };
    (!cached, status, key.target.label.to_string(), key.clone())
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
