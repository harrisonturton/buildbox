//! `bb test`: build targets, then run the tests among them.

use crate::build::{self, Built};
use crate::command::{CommandContext, exit};
use crate::output::Output;
use bazel_analysis::ConfiguredTargetKey;
use bazel_execution::{Backend, TestKey, TestResult, TestStatus};
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
    let epoch = bazel_execution::next_epoch();
    let workspace = context.server.workspaces().get(&context.workspace_root);
    workspace.refresh(crate::command::repo_environment(context, &options)).await;
    let session = workspace.graph.session().await;
    let built = match build::run_command(context, &session, &options, true, out).await {
        Ok(built) => built,
        Err(code) => return build::failed(code, started, out).await,
    };
    build::report(context, &options, &built, out).await;
    if built.errors {
        build::report_stats(started, out).await;
        out.error("Build did NOT complete successfully").await;
        return exit::BUILD_FAILURE;
    }

    let tests: Vec<TestKey> = built
        .targets
        .iter()
        .filter(|(_, target, _)| target.is_test && target.incompatible.is_none())
        .map(|(label, _, _)| TestKey(ConfiguredTargetKey { label: *label, config: built.config }))
        .collect();
    let skipped: Vec<Label> = built
        .targets
        .iter()
        .filter(|(_, target, _)| target.is_test && target.incompatible.is_some())
        .map(|(label, _, _)| *label)
        .collect();
    if tests.is_empty() && skipped.is_empty() {
        out.error("No test targets were found, yet testing was requested").await;
        return NO_TESTS_FOUND;
    }
    let output_mode = options.get("test_output").unwrap_or("summary").to_string();
    let results = run_tests(context, &session, &tests, &built, epoch, &output_mode, out).await;
    drop(session);

    let failed: Vec<TestKey> = results.iter().filter(|(_, result)| !passed(result)).map(|(key, _)| *key).collect();
    workspace.graph.invalidate(&failed).await;
    build::report_stats(started, out).await;
    build::completed(failed.len(), out).await;
    summarize(&results, &skipped, &built, epoch, out).await;
    match failed.is_empty() {
        true => exit::SUCCESS,
        false => TESTS_FAILED,
    }
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
        reported.push((*key, result));
    }
    reported
}

/// Where a test's logs go.
fn testlogs_dir(built: &Built, key: &TestKey) -> PathBuf {
    let config = built.config.get();
    let relative = bazel_analysis::package_dir(key.0.label.package).join_path(bazel_analysis::name_path(key.0.label));
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
    let shown = bazel_query::display_label(key.0.label, &built.main_mapping);
    let failed = result.status != TestStatus::Passed;
    if failed {
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
    let mut ordered: Vec<&(TestKey, Result<Arc<TestResult>, String>)> = results.iter().collect();
    ordered.sort_by_key(|(key, result)| (!passed(result), *key));
    for (key, result) in ordered {
        let (word, prefix, seconds) = match result {
            Ok(result) => {
                let cached = result.cached || result.epoch < epoch;
                executed += usize::from(!cached);
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
        let label = bazel_query::display_label(key.0.label, &built.main_mapping);
        let width = status_width(&label, prefix, word);
        out.stdout(&format!("{label:<width$}{prefix}{word} in {seconds:.1}s\n")).await;
        if word != "PASSED" {
            out.stdout(&format!("  {}\n", testlogs_dir(built, key).join("test.log").display())).await;
        }
    }
    let total = results.len() + skipped.len();
    let tests = if total == 1 { "test" } else { "tests" };
    let mut parts = Vec::new();
    if passes > 0 {
        parts.push(format!("{passes} {} passes", if passes == 1 { "test" } else { "tests" }));
    }
    if failures > 0 {
        parts.push(format!("{failures} fails locally"));
    }
    match skipped.len() {
        0 => {}
        1 => parts.push("1 was skipped".to_string()),
        count => parts.push(format!("{count} were skipped")),
    }
    out.stdout(&format!("\nExecuted {executed} out of {total} {tests}: {}.\n", parts.join(" and "))).await;
}
