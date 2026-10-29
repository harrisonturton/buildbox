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
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The script Bazel runs every test under.
const TEST_SETUP: &str = "@@bazel_tools//tools/test:test-setup.sh";

/// The script that writes a `test.xml` from the log of a test that didn't
/// write one.
const GENERATE_XML: &str = "@@bazel_tools//tools/test:generate-xml.sh";

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
    /// Passed after failed attempts.
    Flaky,
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
    /// The attempts that failed before the last, which a flaky test is
    /// retried after.
    pub failed_attempts: Vec<Attempt>,
    /// The other files the test left in its directory of the testlogs
    /// tree, by their paths in it, such as `test.outputs/outputs.zip`.
    pub outputs: Vec<(String, BlobDigest)>,
    /// The `Tree` of the undeclared outputs it left in `test.outputs`,
    /// unless they're zipped.
    pub undeclared_outputs: Option<BlobDigest>,
}

/// A failed attempt at running a test, whose log and `test.xml` Bazel keeps
/// as `test_attempts/attempt_<n>.log` and `.xml`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Attempt {
    pub status: TestStatus,
    pub exit_code: i32,
    pub log: Option<BlobDigest>,
    pub xml: Option<BlobDigest>,
    pub duration_ms: u64,
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
    /// Which shard and run of the test this is.
    pub run: TestRun,
    /// How many times to try the test before it fails: more than once for
    /// flaky tests.
    pub attempts: u32,
}

/// `--cache_test_results`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub enum CacheResults {
    /// Reuse results, unless the test runs more than once.
    #[default]
    Auto,
    Yes,
    No,
}

/// One shard of a test, in one of the runs `--runs_per_test` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct TestRun {
    /// The shard, from 0.
    pub shard: u32,
    /// How many shards the test is split into; 0 when it isn't sharded.
    pub shards: u32,
    /// The run, from 0.
    pub run: u32,
    /// How many times the test runs.
    pub runs: u32,
}

impl TestRun {
    /// The only run of an unsharded test.
    pub const SINGLE: TestRun = TestRun { shard: 0, shards: 0, run: 0, runs: 1 };

    /// Every shard and run of a test of `shards` shards run `runs` times,
    /// in Bazel's order.
    pub fn all(shards: u32, runs: u32) -> Vec<TestRun> {
        let mut all = Vec::new();
        for shard in 0..shards.max(1) {
            for run in 0..runs.max(1) {
                all.push(TestRun { shard, shards, run, runs: runs.max(1) });
            }
        }
        all
    }

    pub fn is_sharded(&self) -> bool {
        self.shards > 1
    }

    /// The directory of the run's logs in the test's, as Bazel names it:
    /// `shard_1_of_2`, `run_1_of_3`, `shard_1_of_2_run_1_of_3`, or none.
    pub fn dir(&self) -> Option<String> {
        let shard = self.is_sharded().then(|| format!("shard_{}_of_{}", self.shard + 1, self.shards));
        let run = (self.runs > 1).then(|| format!("run_{}_of_{}", self.run + 1, self.runs));
        match (shard, run) {
            (Some(shard), Some(run)) => Some(format!("{shard}_{run}")),
            (shard, run) => shard.or(run),
        }
    }

    /// How Bazel names the run after the test's label: `(shard 1 of 2)`,
    /// `(run 1 of 3)`, `(shard 1 of 2, run 1 of 3)` or nothing.
    pub fn suffix(&self) -> String {
        match (self.is_sharded(), self.runs > 1) {
            (true, true) => format!("(shard {} of {}, run {} of {})", self.shard + 1, self.shards, self.run + 1, self.runs),
            (true, false) => format!("(shard {} of {})", self.shard + 1, self.shards),
            (false, true) => format!("(run {} of {})", self.run + 1, self.runs),
            (false, false) => String::new(),
        }
    }
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
    /// `--cache_test_results`: whether results may come from the action
    /// cache.
    pub cache_results: CacheResults,
    /// Whether undeclared outputs are zipped, as
    /// `--zip_undeclared_test_outputs` says.
    pub zip_outputs: bool,
    /// `--run_under`: the command and its options that tests run under.
    pub run_under: Vec<String>,
}

impl Key for TestKey {
    type Value = TestResult;
    const KIND: &'static str = "TEST";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<TestResult, SkyError> {
        let backend = ctx.global::<Backend>()?;
        let test = TestSpec::of(ctx, self, &self.settings).await?;
        let _shared = match test.exclusive {
            true => None,
            false => Some(EXCLUSIVE.read().await),
        };
        let _alone = match test.exclusive {
            true => Some(EXCLUSIVE.write().await),
            false => None,
        };
        let mut failed_attempts = Vec::new();
        loop {
            let ran = run_attempt(ctx, &backend, self, &test, failed_attempts.len()).await?;
            let last = failed_attempts.len() + 1 >= self.attempts.max(1) as usize;
            if ran.status == TestStatus::Passed || last {
                return Ok(ran.into_result(&test, failed_attempts));
            }
            failed_attempts.push(Attempt {
                status: ran.status,
                exit_code: ran.exit_code,
                log: ran.log,
                xml: ran.xml,
                duration_ms: ran.duration_ms,
            });
        }
    }
}

/// What one attempt at running a test gave.
struct Ran {
    status: TestStatus,
    exit_code: i32,
    duration_ms: u64,
    log: Option<BlobDigest>,
    xml: Option<BlobDigest>,
    outputs: Vec<(String, BlobDigest)>,
    undeclared: Option<BlobDigest>,
    cached: bool,
}

impl Ran {
    fn into_result(self, test: &TestSpec, failed_attempts: Vec<Attempt>) -> TestResult {
        let status = match (self.status, failed_attempts.is_empty()) {
            (TestStatus::Passed, false) => TestStatus::Flaky,
            (status, _) => status,
        };
        TestResult {
            status,
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            log: self.log,
            xml: self.xml,
            cached: self.cached,
            epoch: EPOCH.load(Ordering::Relaxed),
            timeout_seconds: test.category_timeout,
            reusable: test.cacheable && test.read_cache && status != TestStatus::Flaky,
            failed_attempts,
            outputs: self.outputs,
            undeclared_outputs: self.undeclared,
        }
    }
}

/// Run a test once, judging whether it passed as Bazel does: by its exit
/// code, unless it exited early, or didn't say it supports sharding when
/// it's sharded.
async fn run_attempt(ctx: &Ctx, backend: &Backend, key: &TestKey, test: &TestSpec, attempt: usize) -> Result<Ran, SkyError> {
    let request = test.request(ctx, attempt).await?;
    let _turn = crate::jobs::job_turn(ctx).await?;
    let description = format!("Testing {}", test.name());
    let shown = progress(ctx).map(|progress| progress.action_started(test_progress_id(key), Vec::new(), description));
    if let Some(shown) = &shown {
        shown.running(backend.runners.run);
    }
    let started = Instant::now();
    let spawned = execute(backend, request).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let spawned = match spawned {
        Ok(spawned) => spawned,
        Err(error) if error.message().contains("timed out") => return Ok(timed_out(duration_ms)),
        Err(error) => return Err(error),
    };
    finish(shown, backend.runners.name(spawned.cached, false));
    let result = spawned.result;
    let mut log = log_digest(backend, &result).await?;
    let outputs = output_digests(&result, &test.dir);
    let produced = |name: &str| outputs.iter().any(|(path, _)| path == name);
    let mut status = match result.exit_code {
        0 => TestStatus::Passed,
        _ => TestStatus::Failed,
    };
    let mut note = None;
    if status == TestStatus::Passed && produced(PREMATURE_EXIT_FILE) {
        status = TestStatus::Failed;
        note = Some("-- Test exited prematurely (TEST_PREMATURE_EXIT_FILE exists) --\n");
    }
    if status == TestStatus::Passed && key.run.is_sharded() && !produced(SHARD_STATUS_FILE) {
        let location = bazel_loading::target_location(ctx, key.target.label).await;
        let failed = format!("Testing {} failed: {SHARDING_UNSUPPORTED}", test.name());
        return Err(SkyError::new(match location {
            Some(location) => format!("{location}: {failed}"),
            None => failed,
        }));
    }
    if let Some(note) = note {
        log = Some(append_to_log(backend, log, note).await?);
    }
    let undeclared = undeclared_outputs(&result, &test.path(UNDECLARED_OUTPUTS));
    let mut xml = outputs.iter().find(|(path, _)| path == "test.xml").map(|(_, digest)| *digest);
    if let (None, Some(log)) = (xml, log) {
        xml = generate_xml(ctx, backend, test, log, duration_ms, result.exit_code).await?;
    }
    let kept = outputs.into_iter().filter(|(path, _)| !INTERNAL_OUTPUTS.contains(&path.as_str())).collect();
    Ok(Ran {
        status,
        exit_code: result.exit_code,
        duration_ms: match spawned.cached {
            true => 0,
            false => duration_ms,
        },
        log,
        xml,
        outputs: kept,
        undeclared,
        cached: spawned.cached,
    })
}

/// Write a `test.xml` from the log of a test that didn't write one, as
/// Bazel does in an action of its own, from the test's environment, how
/// long it ran and how it exited.
async fn generate_xml(ctx: &Ctx, backend: &Backend, test: &TestSpec, log: BlobDigest, duration_ms: u64, exit_code: i32) -> Result<Option<BlobDigest>, SkyError> {
    let script = Artifact::source(Label::parse_canonical(GENERATE_XML).map_err(|error| SkyError::new(error.to_string()))?);
    let log_path = test.path("test.log");
    let xml_path = test.path("test.xml");
    let mut inputs = BTreeMap::new();
    inputs.insert(TreePath::Exec(script.exec_path), input(ctx, script).await?);
    inputs.insert(TreePath::Text(log_path.clone()), digest_input(log));
    let mut env = test.environment();
    env.push(("TEST_NAME".to_string(), test.name()));
    if !test.run.is_sharded() {
        env.push(("TEST_SHARD_INDEX".to_string(), "0".to_string()));
        env.push(("TEST_TOTAL_SHARDS".to_string(), "0".to_string()));
    }
    env.sort();
    let request = SpawnRequest {
        inputs,
        arguments: vec![script.exec_path.to_string(), log_path, xml_path.clone(), (duration_ms / 1000).to_string(), exit_code.to_string()],
        env,
        output_files: vec![xml_path.clone()],
        output_directories: Vec::new(),
        cacheable: true,
        read_cache: true,
        write_cache: true,
        timeout: None,
        description: format!("Generating test.xml for {}", test.name()),
        platform: test.platform.clone(),
        tools: BTreeSet::new(),
    };
    let spawned = execute(backend, request).await?;
    let file = spawned.result.output_files.iter().find(|file| file.path == xml_path);
    let digest = file.and_then(|file| file.digest.as_option()).and_then(|digest| cas::Digest::from_proto(digest).ok());
    Ok(digest.map(|digest| BlobDigest::from_cas(&digest)))
}

/// An input that's a blob already in the CAS.
fn digest_input(digest: BlobDigest) -> crate::action_key::Input {
    crate::action_key::Input {
        digest,
        executable: false,
        is_directory: false,
        source: None,
        tree: None,
    }
}

/// What Bazel says when a sharded test didn't touch
/// `TEST_SHARD_STATUS_FILE`.
const SHARDING_UNSUPPORTED: &str = "Sharding requested, but the test runner did not advertise support for it by touching TEST_SHARD_STATUS_FILE. Either remove the 'shard_count' attribute, use a test runner that supports sharding or temporarily disable this check via --noincompatible_check_sharding_support.";

/// The file a test that exits before it should leaves.
const PREMATURE_EXIT_FILE: &str = "test.exited_prematurely";

/// The file a sharded test touches to say it supports sharding.
const SHARD_STATUS_FILE: &str = "test.shard";

/// Outputs that only say how the test ran, which the testlogs tree doesn't
/// keep.
const INTERNAL_OUTPUTS: &[&str] = &[PREMATURE_EXIT_FILE, SHARD_STATUS_FILE, "test.xml"];

/// Where a test leaves the outputs it doesn't declare.
pub const UNDECLARED_OUTPUTS: &str = "test.outputs";

/// The `Tree` of the directory of undeclared outputs at `path`, if the
/// test left it as a directory.
fn undeclared_outputs(result: &remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult, path: &str) -> Option<BlobDigest> {
    let directory = result.output_directories.iter().find(|directory| directory.path == path)?;
    let digest = directory.tree_digest.as_option().and_then(|digest| cas::Digest::from_proto(digest).ok())?;
    Some(BlobDigest::from_cas(&digest))
}

/// The digests of the files the test left in `dir`, by their paths in it.
fn output_digests(result: &remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult, dir: &str) -> Vec<(String, BlobDigest)> {
    let prefix = format!("{dir}/");
    let mut outputs = Vec::new();
    for file in &result.output_files {
        let Some(path) = file.path.strip_prefix(&prefix) else {
            continue;
        };
        let digest = file.digest.as_option().and_then(|digest| cas::Digest::from_proto(digest).ok());
        if let Some(digest) = digest {
            outputs.push((path.to_string(), BlobDigest::from_cas(&digest)));
        }
    }
    outputs
}

/// A test log with `note` after what the test printed.
async fn append_to_log(backend: &Backend, log: Option<BlobDigest>, note: &str) -> Result<BlobDigest, SkyError> {
    let mut bytes = match log {
        Some(log) => backend.cas.read_bytes(&log.to_cas()).await.map_err(cas_error)?,
        None => Vec::new(),
    };
    bytes.extend_from_slice(note.as_bytes());
    let digest = backend.cas.put_bytes(&bytes).await.map_err(cas_error)?;
    Ok(BlobDigest::from_cas(&digest))
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

fn timed_out(duration_ms: u64) -> Ran {
    Ran {
        status: TestStatus::TimedOut,
        exit_code: -1,
        duration_ms,
        log: None,
        xml: None,
        outputs: Vec::new(),
        undeclared: None,
        cached: false,
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
    run: TestRun,
    /// The directory of this run's logs and outputs.
    dir: String,
    zip_outputs: bool,
    executable: Artifact,
    runfiles: Vec<(String, Artifact)>,
    /// The empty files of its runfiles tree.
    empty_runfiles: Vec<String>,
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
    /// Whether its result may be cached at all.
    cacheable: bool,
    /// Whether its result may come from the action cache, as
    /// `--cache_test_results` says for it.
    read_cache: bool,
    /// Whether it must run while no other test does.
    exclusive: bool,
    /// The properties of the platform it runs on.
    platform: Vec<(String, String)>,
    /// The command it runs under, and its options.
    run_under: Vec<String>,
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
        let dir = match test.run.dir() {
            Some(run) => format!("{testlogs}/{run}"),
            None => testlogs,
        };
        Ok(TestSpec {
            label: key.label,
            run: test.run,
            dir,
            zip_outputs: settings.zip_outputs,
            executable,
            runfiles,
            empty_runfiles,
            timeout: Duration::from_secs(timeout_seconds(timeout.as_deref(), &size, &settings.timeouts)),
            category_timeout: timeout_seconds(timeout.as_deref(), &size, &[]),
            size,
            args,
            env,
            action_env: config.options.action_env.clone(),
            test_env: settings.env.clone(),
            filter: settings.filter.clone(),
            cacheable: !tagged(&["external"]),
            read_cache: match settings.cache_results {
                CacheResults::Yes => true,
                CacheResults::No => false,
                CacheResults::Auto => test.run.runs <= 1,
            },
            exclusive: tagged(&["exclusive", "exclusive-if-local"]),
            platform: target.test.as_ref().map(|test| test.exec_properties.clone()).unwrap_or_default(),
            run_under: settings.run_under.clone(),
        })
    }

    /// A file of this run's, in its directory of the testlogs tree.
    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.dir)
    }

    /// The test's name in progress and failures: its label, and which
    /// shard and run this is.
    fn name(&self) -> String {
        match self.run.suffix().as_str() {
            "" => self.label.to_string(),
            suffix => format!("{} {suffix}", self.label),
        }
    }

    /// The files the test may leave, as Bazel collects them.
    fn output_files(&self) -> Vec<String> {
        let mut names = vec![
            "test.xml",
            PREMATURE_EXIT_FILE,
            "test.warnings",
            "test.raw_splitlogs/test.splitlogs",
            "test.unused_runfiles_log",
            "test.infrastructure_failure",
            "test.outputs_manifest/MANIFEST",
            "test.outputs_manifest/ANNOTATIONS",
            "test.outputs_manifest/ANNOTATIONS.pb",
        ];
        if self.run.is_sharded() {
            names.push(SHARD_STATUS_FILE);
        }
        if self.zip_outputs {
            names.push("test.outputs/outputs.zip");
        }
        names.into_iter().map(|name| self.path(name)).collect()
    }

    /// What comes between test-setup.sh and the test's arguments: the
    /// test's path in its runfiles, after `--run_under`'s command and
    /// options if there are some. As Bazel runs them, a command without a
    /// `/`, which may be a shell builtin or on `PATH`, runs in a shell, and
    /// the test's path is written so it isn't looked up on `PATH`.
    fn run_under_arguments(&self) -> Vec<String> {
        let path = self.executable.short_path();
        let Some(command) = self.run_under.first() else {
            return vec![path];
        };
        let mut arguments = Vec::new();
        if !command.contains('/') {
            arguments.extend(["/bin/bash", "-c", "\"$@\"", "/bin/bash"].map(str::to_string));
        }
        arguments.extend(self.run_under.iter().cloned());
        arguments.push(match path.contains('/') {
            true => path,
            false => format!("./{path}"),
        });
        arguments
    }

    fn srcdir(&self) -> String {
        format!("{}.runfiles", self.executable.exec_path)
    }

    /// The command that runs the test under test-setup.sh, as Bazel does.
    /// Retries aren't cached, so that a flaky test runs again next time.
    async fn request(&self, ctx: &Ctx, attempt: usize) -> Result<SpawnRequest, SkyError> {
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

        let mut arguments = vec![setup.exec_path.to_string()];
        arguments.extend(self.run_under_arguments());
        arguments.extend(self.args.iter().cloned());
        Ok(SpawnRequest {
            inputs,
            arguments,
            env: self.environment(),
            output_files: self.output_files(),
            output_directories: match self.zip_outputs {
                true => Vec::new(),
                false => vec![self.path(UNDECLARED_OUTPUTS)],
            },
            cacheable: self.cacheable,
            read_cache: self.read_cache && attempt == 0,
            write_cache: attempt == 0,
            timeout: Some(self.timeout),
            description: format!("Testing {}", self.name()),
            platform: self.platform.clone(),
            tools: BTreeSet::new(),
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
            ("XML_OUTPUT_FILE", self.path("test.xml")),
            ("TEST_UNDECLARED_OUTPUTS_DIR", self.path("test.outputs")),
            ("TEST_UNDECLARED_OUTPUTS_MANIFEST", self.path("test.outputs_manifest/MANIFEST")),
            ("TEST_UNDECLARED_OUTPUTS_ANNOTATIONS", self.path("test.outputs_manifest/ANNOTATIONS")),
            ("TEST_UNDECLARED_OUTPUTS_ANNOTATIONS_DIR", self.path("test.outputs_manifest")),
            ("TEST_PREMATURE_EXIT_FILE", self.path(PREMATURE_EXIT_FILE)),
            ("TEST_INFRASTRUCTURE_FAILURE_FILE", self.path("test.infrastructure_failure")),
            ("TEST_WARNINGS_OUTPUT_FILE", self.path("test.warnings")),
            ("TEST_UNUSED_RUNFILES_LOG_FILE", self.path("test.unused_runfiles_log")),
            ("TEST_LOGSPLITTER_OUTPUT_FILE", self.path("test.raw_splitlogs/test.splitlogs")),
            ("JAVA_RUNFILES", srcdir.clone()),
            ("PYTHON_RUNFILES", srcdir.clone()),
            ("RUNFILES_DIR", srcdir),
        ];
        env.extend(fixed.into_iter().map(|(name, value)| (name.to_string(), value)));
        if self.zip_outputs {
            env.insert("TEST_UNDECLARED_OUTPUTS_ZIP".to_string(), self.path("test.outputs/outputs.zip"));
        }
        if self.run.is_sharded() {
            env.insert("TEST_SHARD_INDEX".to_string(), self.run.shard.to_string());
            env.insert("TEST_TOTAL_SHARDS".to_string(), self.run.shards.to_string());
            env.insert("TEST_SHARD_STATUS_FILE".to_string(), self.path(SHARD_STATUS_FILE));
        }
        if self.run.runs > 1 {
            env.entry("TEST_RANDOM_SEED".to_string()).or_insert_with(|| (self.run.run + 1).to_string());
            env.entry("TEST_RUN_NUMBER".to_string()).or_insert_with(|| (self.run.run + 1).to_string());
        }
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
