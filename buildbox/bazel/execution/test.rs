//! Running tests.

use crate::action_key::{blob_input, cas_error, input, progress};
use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::spawn::{SpawnRequest, TreePath, execute, finish};
use bazel_analysis::{Artifact, ConfiguredTargetKey, FileValue, Runfiles, runfiles_repo_name};
use bazel_label::{Label, RepoName};
use bazel_loading::PackageKey;
use bazel_packages::{AttrValue, Target};
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Compute, Ctx, Key, SkyError, encoded_value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The script Bazel runs every test under.
const TEST_SETUP: &str = "@@bazel_tools//tools/test:test-setup.sh";

/// Counts commands, so a test result can tell whether this command ran it.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Start a new command's epoch, returning it.
pub fn next_epoch() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed) + 1
}

/// How a test run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum TestStatus {
    Passed,
    Failed,
    TimedOut,
}

/// The result of running a test.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TestResult {
    pub status: TestStatus,
    pub exit_code: i32,
    /// How long it ran, in milliseconds.
    pub duration_ms: u64,
    /// What it printed, which becomes its `test.log`.
    pub log: Option<BlobDigest>,
    /// The `test.xml` it wrote, or test-setup.sh wrote for it.
    pub xml: Option<BlobDigest>,
    /// Whether the result came from the action cache.
    pub cached: bool,
    /// The epoch of the command that ran it.
    pub epoch: u64,
    /// The default timeout, in seconds, of the category its size or
    /// `timeout` attribute puts it in, which says whether it's sized well.
    pub timeout_seconds: u64,
    /// Whether the result may be reused by later commands, as it can unless
    /// the test is tagged `external` or `--cache_test_results` is off.
    pub reusable: bool,
}

impl TestResult {
    /// Whether it passed in much less time than its timeout allows, or
    /// nearly timed out, so its size or timeout could be better, as Bazel
    /// judges: outside of half the next shorter timeout and 90% of its own.
    pub fn wrong_size(&self) -> bool {
        let seconds = self.duration_ms / 1000;
        let (shorter, longest) = match self.timeout_seconds {
            60 => (0, false),
            300 => (60, false),
            900 => (300, false),
            _ => (900, true),
        };
        let max = match longest {
            true => 365 * 24 * 60 * 60,
            false => self.timeout_seconds * 9 / 10,
        };
        seconds < shorter / 2 || seconds >= max
    }
}

encoded_value!(TestResult);

/// Runs a test target, with the command's [`TestSettings`].
///
/// Results depend on more than the graph sees, such as the time a test
/// takes, so the key is external: a failed test's result is invalidated so
/// that it runs again, and passing results are reused from the action
/// cache rather than from other workspaces' graphs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct TestKey {
    pub target: ConfiguredTargetKey,
    pub settings: TestSettings,
    /// The values the client's environment gives the variables the test's
    /// `RunEnvironmentInfo` inherits, by name; unset ones are left out.
    pub inherited_env: Vec<(String, String)>,
}

/// How a command runs tests, as its test options say.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct TestSettings {
    /// `--test_filter`, which the test gets as `TESTBRIDGE_TEST_ONLY`.
    pub filter: Option<String>,
    /// `--test_arg`s, which follow the rule's `args`.
    pub args: Vec<String>,
    /// `--test_env`'s variables, with inherited ones' values.
    pub env: Vec<(String, String)>,
    /// `--test_timeout`: the timeouts of short, moderate, long and eternal
    /// tests in seconds, where they aren't Bazel's defaults.
    pub timeouts: Vec<Option<u64>>,
    /// Whether results may come from the action cache, as they may unless
    /// `--cache_test_results` is off.
    pub cached: bool,
}

impl Key for TestKey {
    type Value = TestResult;
    const KIND: &'static str = "TEST";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<TestResult, SkyError> {
        let backend = ctx.global::<Backend>()?;
        let test = TestSpec::of(ctx, self, &self.settings).await?;
        let request = test.request(ctx).await?;
        let _shared = match test.exclusive {
            true => None,
            false => Some(EXCLUSIVE.read().await),
        };
        let _alone = match test.exclusive {
            true => Some(EXCLUSIVE.write().await),
            false => None,
        };
        let _turn = crate::jobs::job_turn(ctx).await?;
        let shown = progress(ctx).map(|progress| progress.action_started(test_progress_id(self), Vec::new(), format!("Testing {}", self.target.label)));
        if let Some(shown) = &shown {
            shown.running(backend.runners.run);
        }
        let started = Instant::now();
        let spawned = execute(&backend, request).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(error) if error.message().contains("timed out") => return Ok(timed_out(duration_ms, test.category_timeout, test.cacheable)),
            Err(error) => return Err(error),
        };
        finish(shown, backend.runners.name(spawned.cached));
        let result = spawned.result;
        let log = log_digest(&backend, &result).await?;
        let xml_path = test.xml_path();
        let xml = result
            .output_files
            .iter()
            .find(|file| file.path == xml_path)
            .and_then(|file| file.digest.as_option())
            .and_then(|digest| cas::Digest::from_proto(digest).ok())
            .map(|digest| BlobDigest::from_cas(&digest));
        let status = match result.exit_code {
            0 => TestStatus::Passed,
            _ => TestStatus::Failed,
        };
        Ok(TestResult {
            status,
            exit_code: result.exit_code,
            duration_ms: match spawned.cached {
                true => 0,
                false => duration_ms,
            },
            log,
            xml,
            cached: spawned.cached,
            epoch: EPOCH.load(Ordering::Relaxed),
            timeout_seconds: test.category_timeout,
            reusable: test.cacheable,
        })
    }
}

/// Held while a test runs: shared by tests that may run alongside others,
/// and held alone by those tagged `exclusive`.
static EXCLUSIVE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// A number identifying a test run in progress reports.
fn test_progress_id(key: &TestKey) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(key, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn timed_out(duration_ms: u64, timeout_seconds: u64, reusable: bool) -> TestResult {
    TestResult {
        status: TestStatus::TimedOut,
        exit_code: -1,
        duration_ms,
        log: None,
        xml: None,
        cached: false,
        epoch: EPOCH.load(Ordering::Relaxed),
        timeout_seconds,
        reusable,
    }
}

/// What printed output the test left: its stdout, into which test-setup.sh
/// merges stderr.
async fn log_digest(backend: &Backend, result: &remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult) -> Result<Option<BlobDigest>, SkyError> {
    if !result.stdout_raw.is_empty() {
        let digest = backend.cas.put_bytes(&result.stdout_raw).await.map_err(cas_error)?;
        return Ok(Some(BlobDigest::from_cas(&digest)));
    }
    let digest = result.stdout_digest.as_option().and_then(|digest| cas::Digest::from_proto(digest).ok());
    Ok(digest.map(|digest| BlobDigest::from_cas(&digest)))
}

/// What running a test needs: its executable and runfiles, and the
/// attributes of its rule that say how to run it.
struct TestSpec {
    label: Label,
    executable: Artifact,
    runfiles: Vec<(String, Artifact)>,
    /// The empty files of its runfiles tree.
    empty_runfiles: Vec<String>,
    testlogs: String,
    size: String,
    timeout: Duration,
    /// The default timeout of its category, in seconds.
    category_timeout: u64,
    args: Vec<String>,
    env: Vec<(String, String)>,
    action_env: Vec<(String, String)>,
    /// `--test_env`'s variables.
    test_env: Vec<(String, String)>,
    filter: Option<String>,
    /// Whether its result may come from the action cache.
    cacheable: bool,
    /// Whether it must run while no other test does.
    exclusive: bool,
}

impl TestSpec {
    async fn of(ctx: &Ctx, test: &TestKey, settings: &TestSettings) -> Result<TestSpec, SkyError> {
        let key = test.target;
        let target = ctx.compute(&key).await?;
        let providers = target.providers()?;
        let value = providers.get().ok_or_else(|| SkyError::new(format!("{} has no providers", key.label)))?;
        let executable = value
            .default_info("executable")
            .and_then(FileValue::artifact)
            .ok_or_else(|| SkyError::new(format!("{} is a test without an executable", key.label)))?;
        let workspace_dir = runfiles_repo_name(RepoName::MAIN);
        let default_runfiles = value.default_info("default_runfiles").and_then(Runfiles::from_value);
        let runfiles = default_runfiles.map(|runfiles| runfiles.entries(workspace_dir)).unwrap_or_default();
        let empty_runfiles = default_runfiles.map(|runfiles| runfiles.empty_files(workspace_dir)).unwrap_or_default();
        let env = target.run_env.resolve(|name| test.inherited_env.iter().find(|(inherited, _)| inherited == name).map(|(_, value)| value.clone()));
        let package = ctx.compute(&PackageKey(key.label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(key.label.name) else {
            return Err(SkyError::new(format!("{} is not a rule", key.label)));
        };
        let size = string_attr(rule.attr(Symbol::intern("size"))).unwrap_or_else(|| "medium".to_string());
        let timeout = string_attr(rule.attr(Symbol::intern("timeout")));
        let mut args = target.args.clone();
        args.extend(settings.args.iter().cloned());
        let tags = match rule.attr(Symbol::intern("tags")) {
            Some(AttrValue::StringList(tags)) => tags.clone(),
            _ => Vec::new(),
        };
        let tagged = |wanted: &[&str]| tags.iter().any(|tag| wanted.contains(&tag.as_str()));
        if tagged(&["block-network"]) {
            return Err(SkyError::new(format!("{} is tagged 'block-network', but bb can't block tests' network yet", key.label)));
        }
        let config = key.config.get();
        let testlogs = format!(
            "bazel-out/{}/testlogs/{}",
            config.output_directory,
            bazel_analysis::package_dir(key.label.package).join_path(bazel_analysis::name_path(key.label))
        );
        Ok(TestSpec {
            label: key.label,
            executable,
            runfiles,
            empty_runfiles,
            testlogs,
            timeout: Duration::from_secs(timeout_seconds(timeout.as_deref(), &size, &settings.timeouts)),
            category_timeout: timeout_seconds(timeout.as_deref(), &size, &[]),
            size,
            args,
            env,
            action_env: config.options.action_env.clone(),
            test_env: settings.env.clone(),
            filter: settings.filter.clone(),
            cacheable: !tagged(&["external"]) && settings.cached,
            exclusive: tagged(&["exclusive", "exclusive-if-local"]),
        })
    }

    fn xml_path(&self) -> String {
        format!("{}/test.xml", self.testlogs)
    }

    fn srcdir(&self) -> String {
        format!("{}.runfiles", self.executable.exec_path)
    }

    /// The command that runs the test under test-setup.sh, as Bazel does.
    async fn request(&self, ctx: &Ctx) -> Result<SpawnRequest, SkyError> {
        let setup = Artifact::source(Label::parse_canonical(TEST_SETUP).map_err(|error| SkyError::new(error.to_string()))?);
        let mut inputs = BTreeMap::new();
        inputs.insert(TreePath::Exec(setup.exec_path), input(ctx, setup).await?);
        inputs.insert(TreePath::Exec(self.executable.exec_path), input(ctx, self.executable).await?);
        let srcdir = self.srcdir();
        for (path, artifact) in &self.runfiles {
            inputs.insert(TreePath::Text(format!("{srcdir}/{path}")), input(ctx, *artifact).await?);
        }
        if !self.empty_runfiles.is_empty() {
            let empty = blob_input(ctx, &[]).await?;
            for path in &self.empty_runfiles {
                inputs.insert(TreePath::Text(format!("{srcdir}/{path}")), empty.clone());
            }
        }
        let mapping = repo_mapping(ctx, &self.runfiles).await?;
        inputs.insert(TreePath::Text(format!("{srcdir}/_repo_mapping")), blob_input(ctx, mapping.as_bytes()).await?);

        let mut arguments = vec![setup.exec_path.to_string(), self.executable.short_path()];
        arguments.extend(self.args.iter().cloned());
        Ok(SpawnRequest {
            inputs,
            arguments,
            env: self.environment(),
            output_files: vec![self.xml_path()],
            output_directories: Vec::new(),
            cacheable: self.cacheable,
            timeout: Some(self.timeout),
            description: format!("Testing {}", self.label),
        })
    }

    /// The environment Bazel gives tests: `--action_env`'s, then
    /// `--test_env`'s, its own and the test runner's.
    fn environment(&self) -> Vec<(String, String)> {
        let srcdir = self.srcdir();
        let mut env: BTreeMap<String, String> = self.action_env.iter().cloned().collect();
        env.extend(self.test_env.iter().cloned());
        let fixed = [
            ("TEST_TARGET", self.label.to_string()),
            ("TEST_SIZE", self.size.clone()),
            ("TEST_TIMEOUT", self.timeout.as_secs().to_string()),
            ("TEST_WORKSPACE", runfiles_repo_name(RepoName::MAIN).to_string()),
            ("TEST_SRCDIR", srcdir.clone()),
            ("TEST_BINARY", self.executable.short_path()),
            ("TEST_TMPDIR", "_tmp".to_string()),
            ("XML_OUTPUT_FILE", self.xml_path()),
            ("TEST_UNDECLARED_OUTPUTS_DIR", format!("{}/test.outputs", self.testlogs)),
            ("TEST_PREMATURE_EXIT_FILE", format!("{}/test.exited_prematurely", self.testlogs)),
            ("TEST_INFRASTRUCTURE_FAILURE_FILE", format!("{}/test.infrastructure_failure", self.testlogs)),
            ("TEST_WARNINGS_OUTPUT_FILE", format!("{}/test.warnings", self.testlogs)),
            ("JAVA_RUNFILES", srcdir.clone()),
            ("PYTHON_RUNFILES", srcdir.clone()),
            ("RUNFILES_DIR", srcdir),
        ];
        env.extend(fixed.into_iter().map(|(name, value)| (name.to_string(), value)));
        env.extend(self.env.iter().cloned());
        if let Some(filter) = &self.filter {
            env.insert("TESTBRIDGE_TEST_ONLY".to_string(), filter.clone());
        }
        env.into_iter().collect()
    }
}

/// The repository mapping manifest of a runfiles tree: how each repository
/// with runfiles sees the others that have runfiles.
pub async fn repo_mapping(compute: &impl Compute, runfiles: &[(String, Artifact)]) -> Result<String, SkyError> {
    bazel_analysis::repo_mapping_manifest(compute, runfiles.iter().map(|(_, artifact)| *artifact), None).await
}

fn string_attr(value: Option<&AttrValue>) -> Option<String> {
    match value {
        Some(AttrValue::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

/// A test's timeout: from its `timeout` attribute, or its size, and
/// `--test_timeout`'s `overrides`, by category.
fn timeout_seconds(timeout: Option<&str>, size: &str, overrides: &[Option<u64>]) -> u64 {
    let category = timeout.unwrap_or(match size {
        "small" => "short",
        "large" => "long",
        "enormous" => "eternal",
        _ => "moderate",
    });
    let (index, default) = match category {
        "short" => (0, 60),
        "long" => (2, 900),
        "eternal" => (3, 3600),
        _ => (1, 300),
    };
    overrides.get(index).copied().flatten().unwrap_or(default)
}
