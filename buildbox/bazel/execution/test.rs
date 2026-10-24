//! Running tests.

use crate::action_key::{Input, cas_error, input, progress};
use crate::backend::Backend;
use crate::digest::BlobDigest;
use crate::spawn::{SpawnRequest, execute, finish};
use bazel_analysis::{Artifact, ConfiguredTargetKey, FileValue, Runfiles, runfiles_repo_name};
use bazel_label::{Label, RepoName};
use bazel_loading::{PackageKey, RepoMappingKey};
use bazel_packages::{AttrValue, Target};
use bazel_starlark::provider::ProviderInstance;
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Compute, Ctx, Key, SkyError, encoded_value};
use std::collections::{BTreeMap, BTreeSet};
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
}

encoded_value!(TestResult);

/// Runs a test target.
///
/// Results depend on more than the graph sees, such as the time a test
/// takes, so the key is external: a failed test's result is invalidated so
/// that it runs again, and passing results are reused from the action
/// cache rather than from other workspaces' graphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct TestKey(pub ConfiguredTargetKey);

impl Key for TestKey {
    type Value = TestResult;
    const KIND: &'static str = "TEST";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<TestResult, SkyError> {
        let backend = ctx.global::<Backend>()?;
        let test = TestSpec::of(ctx, self.0).await?;
        let request = test.request(ctx).await?;
        let shown = progress(ctx).map(|progress| progress.action_started(test_progress_id(self), Vec::new(), format!("Testing {}", self.0.label)));
        if let Some(shown) = &shown {
            shown.running(backend.runners.run);
        }
        let started = Instant::now();
        let spawned = execute(&backend, request).await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let spawned = match spawned {
            Ok(spawned) => spawned,
            Err(error) if error.message().contains("timed out") => return Ok(timed_out(duration_ms)),
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
        })
    }
}

/// A number identifying a test run in progress reports.
fn test_progress_id(key: &TestKey) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(key, &mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn timed_out(duration_ms: u64) -> TestResult {
    TestResult {
        status: TestStatus::TimedOut,
        exit_code: -1,
        duration_ms,
        log: None,
        xml: None,
        cached: false,
        epoch: EPOCH.load(Ordering::Relaxed),
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
    testlogs: String,
    size: String,
    timeout: Duration,
    args: Vec<String>,
    env: Vec<(String, String)>,
    action_env: Vec<(String, String)>,
}

impl TestSpec {
    async fn of(ctx: &Ctx, key: ConfiguredTargetKey) -> Result<TestSpec, SkyError> {
        let target = ctx.compute(&key).await?;
        let providers = target.providers()?;
        let value = providers.get().ok_or_else(|| SkyError::new(format!("{} has no providers", key.label)))?;
        let executable = value
            .default_info("executable")
            .and_then(FileValue::artifact)
            .ok_or_else(|| SkyError::new(format!("{} is a test without an executable", key.label)))?;
        let runfiles = value
            .default_info("default_runfiles")
            .and_then(Runfiles::from_value)
            .map(|runfiles| runfiles.entries(runfiles_repo_name(RepoName::MAIN)))
            .unwrap_or_default();
        let mut env = run_environment(value.provider("RunEnvironmentInfo"));
        let package = ctx.compute(&PackageKey(key.label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(key.label.name) else {
            return Err(SkyError::new(format!("{} is not a rule", key.label)));
        };
        let size = string_attr(rule.attr(Symbol::intern("size"))).unwrap_or_else(|| "medium".to_string());
        let timeout = string_attr(rule.attr(Symbol::intern("timeout")));
        let args = match rule.attr(Symbol::intern("args")) {
            Some(AttrValue::StringList(args)) => args.clone(),
            _ => Vec::new(),
        };
        if let Some(AttrValue::StringDict(entries)) = rule.attr(Symbol::intern("env")) {
            env.extend(entries.iter().cloned());
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
            testlogs,
            timeout: Duration::from_secs(timeout_seconds(timeout.as_deref(), &size)),
            size,
            args,
            env,
            action_env: config.options.action_env.clone(),
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
        inputs.insert(setup.exec_path.to_string(), input(ctx, setup).await?);
        inputs.insert(self.executable.exec_path.to_string(), input(ctx, self.executable).await?);
        let srcdir = self.srcdir();
        for (path, artifact) in &self.runfiles {
            inputs.insert(format!("{srcdir}/{path}"), input(ctx, *artifact).await?);
        }
        let mapping = repo_mapping(ctx, &self.runfiles).await?;
        inputs.insert(format!("{srcdir}/_repo_mapping"), blob_input(ctx, mapping.as_bytes()).await?);

        let mut arguments = vec![setup.exec_path.to_string(), self.executable.short_path()];
        arguments.extend(self.args.iter().cloned());
        Ok(SpawnRequest {
            inputs,
            arguments,
            env: self.environment(),
            output_files: vec![self.xml_path()],
            output_directories: Vec::new(),
            cacheable: true,
            timeout: Some(self.timeout),
            description: format!("Testing {}", self.label),
        })
    }

    /// The environment Bazel gives tests.
    fn environment(&self) -> Vec<(String, String)> {
        let srcdir = self.srcdir();
        let mut env: BTreeMap<String, String> = self.action_env.iter().cloned().collect();
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
        env.into_iter().collect()
    }
}

/// An input with fixed contents, stored in the CAS.
async fn blob_input(ctx: &Ctx, contents: &[u8]) -> Result<Input, SkyError> {
    let backend = ctx.global::<Backend>()?;
    let digest = backend.cas.put_bytes(contents).await.map_err(cas_error)?;
    Ok(Input {
        digest: BlobDigest::from_cas(&digest),
        executable: false,
        is_directory: false,
        source: None,
        tree: None,
    })
}

/// The repository mapping manifest of a runfiles tree: how each repository
/// with runfiles sees the others that have runfiles.
pub async fn repo_mapping(compute: &impl Compute, runfiles: &[(String, Artifact)]) -> Result<String, SkyError> {
    let repos: BTreeSet<RepoName> = runfiles.iter().map(|(_, artifact)| artifact.owner.repo()).chain([RepoName::MAIN]).collect();
    let mut lines = Vec::new();
    for repo in &repos {
        let mapping = compute.compute(&RepoMappingKey(*repo)).await?;
        for (apparent, target) in &mapping.0.entries {
            if apparent.is_empty() || !repos.contains(target) {
                continue;
            }
            lines.push(format!("{},{apparent},{}", repo.as_str(), runfiles_repo_name(*target)));
        }
    }
    lines.sort();
    lines.dedup();
    Ok(lines.into_iter().map(|line| line + "\n").collect())
}

/// The environment a `RunEnvironmentInfo` provider gives.
fn run_environment(info: Option<starlark::values::Value<'_>>) -> Vec<(String, String)> {
    let Some(environment) = info.and_then(ProviderInstance::from_value).and_then(|info| info.field("environment")) else {
        return Vec::new();
    };
    let Some(dict) = starlark::values::dict::DictRef::from_value(environment) else {
        return Vec::new();
    };
    dict.iter()
        .filter_map(|(key, value)| Some((key.unpack_str()?.to_string(), value.unpack_str()?.to_string())))
        .collect()
}

fn string_attr(value: Option<&AttrValue>) -> Option<String> {
    match value {
        Some(AttrValue::String(value)) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

/// A test's timeout: from its `timeout` attribute, or its size.
fn timeout_seconds(timeout: Option<&str>, size: &str) -> u64 {
    let category = timeout.unwrap_or(match size {
        "small" => "short",
        "large" => "long",
        "enormous" => "eternal",
        _ => "moderate",
    });
    match category {
        "short" => 60,
        "long" => 900,
        "eternal" => 3600,
        _ => 300,
    }
}
