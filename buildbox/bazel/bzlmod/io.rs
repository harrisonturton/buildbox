//! What repository rules and module extensions can do to the machine:
//! read and write files, download and extract archives, and run programs.
//! `repository_ctx` and `module_ctx` share these methods.

use crate::graph_access::GraphAccess;
use crate::path_value::PathValue;
use allocative::Allocative;
use bazel_fetch::{ArchiveType, DownloadCache, Integrity, RequestHeaders};
use bazel_starlark::label_value::LabelValue;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::none::{NoneOr, NoneType};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, StringValue, Value, ValueLike, starlark_value};
use std::fmt;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use skycodec::{Decode, Encode};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What a context's I/O methods work with.
pub struct IoState {
    /// The directory relative paths are relative to: the repository being
    /// fetched, or a module extension's working directory.
    pub base_dir: PathBuf,
    pub graph: GraphAccess,
    pub downloads: DownloadCache,
    pub env: HashMap<String, String>,
    /// Where fetched repositories are, whose files may be watched.
    pub repos_dir: PathBuf,
    /// Whether files outside the workspace and repositories may be watched,
    /// as repository rules' may and module extensions' may not.
    pub watch_outside: bool,
    /// What the code read that it must run again for if it changes.
    pub recorded: Arc<Mutex<RecordedInputs>>,
}

/// What a repository rule or module extension read that it must run again
/// for if it changes, beyond what identifies it: as Bazel records them, the
/// environment variables it read, and the files and trees it watched.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct RecordedInputs {
    pub env: Vec<(String, Option<String>)>,
    /// Files, by absolute path, with their digests, `None` if missing.
    pub files: Vec<(String, Option<String>)>,
    /// Directory trees, by absolute path, with their digests.
    pub trees: Vec<(String, String)>,
}

impl RecordedInputs {
    /// What a fetched repository's marker records, or nothing if it's from
    /// a fetch that recorded nothing.
    pub fn decode(bytes: &[u8]) -> RecordedInputs {
        skycodec::from_bytes(bytes).unwrap_or_default()
    }

    pub fn encode(&self) -> Vec<u8> {
        skycodec::to_bytes(self)
    }

    /// Whether everything recorded is as it was, as `graph` sees it now;
    /// what `graph` is computing depends on each.
    pub fn is_current(&self, graph: &GraphAccess) -> bool {
        let env = self.env.iter().all(|(name, value)| graph.env_var(name).is_ok_and(|now| now == *value));
        let files = self.files.iter().all(|(path, digest)| graph.file_digest(Path::new(path)).is_ok_and(|now| now == *digest));
        let trees = self.trees.iter().all(|(path, digest)| graph.tree_digest(Path::new(path)).is_ok_and(|now| now == *digest));
        env && files && trees
    }
}

/// Whether to watch a file that code reads: `yes`, `no`, or `auto`, which
/// watches it if it may be watched.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Watch {
    Yes,
    No,
    Auto,
}

impl Watch {
    fn parse(value: Option<&str>) -> anyhow::Result<Watch> {
        match value.unwrap_or("auto") {
            "yes" => Ok(Watch::Yes),
            "no" => Ok(Watch::No),
            "auto" => Ok(Watch::Auto),
            other => anyhow::bail!("bad value for 'watch' parameter; want 'yes', 'no', or 'auto', got {other}"),
        }
    }
}

/// A Starlark value with I/O methods.
pub trait HasIo {
    fn io(&self) -> &IoState;
}

/// The I/O state of `value`, a `repository_ctx` or `module_ctx`.
pub fn io_of<'v>(value: Value<'v>) -> anyhow::Result<&'v IoState> {
    if let Some(ctx) = value.downcast_ref::<crate::repo_ctx::RepositoryCtx>() {
        return Ok(ctx.io());
    }
    if let Some(ctx) = crate::module_ctx::ModuleCtx::from_value(value) {
        return Ok(ctx.io());
    }
    anyhow::bail!("expected a repository_ctx or module_ctx")
}

impl IoState {
    /// An environment variable, recorded as read.
    fn getenv(&self, name: &str) -> anyhow::Result<Option<String>> {
        let value = self.graph.env_var(name)?;
        let mut recorded = self.recorded();
        if !recorded.env.iter().any(|(recorded_name, _)| recorded_name == name) {
            recorded.env.push((name.to_string(), value.clone()));
        }
        Ok(value)
    }

    /// Watch a file, as `watch` says, as Bazel decides: never one in the
    /// directory being worked in, and one outside the workspace and the
    /// repositories only where that's allowed.
    fn watch_file(&self, path: &Path, watch: Watch) -> anyhow::Result<()> {
        if !self.may_watch(path, watch)? {
            return Ok(());
        }
        let digest = self.graph.file_digest(path)?;
        let path = path.display().to_string();
        let mut recorded = self.recorded();
        if !recorded.files.iter().any(|(recorded_path, _)| *recorded_path == path) {
            recorded.files.push((path, digest));
        }
        Ok(())
    }

    /// Watch a directory tree, every file and directory in it.
    fn watch_tree(&self, path: &Path) -> anyhow::Result<()> {
        if !self.may_watch(path, Watch::Yes)? {
            return Ok(());
        }
        let digest = self.graph.tree_digest(path)?;
        let path = path.display().to_string();
        let mut recorded = self.recorded();
        if !recorded.trees.iter().any(|(recorded_path, _)| *recorded_path == path) {
            recorded.trees.push((path, digest));
        }
        Ok(())
    }

    fn may_watch(&self, path: &Path, watch: Watch) -> anyhow::Result<bool> {
        if watch == Watch::No {
            return Ok(false);
        }
        if path.starts_with(&self.base_dir) {
            if watch == Watch::Auto {
                return Ok(false);
            }
            anyhow::bail!("attempted to watch path under working directory");
        }
        let in_workspace = path.starts_with(self.graph.workspace_root()?) || path.starts_with(&self.repos_dir);
        if !in_workspace && !self.watch_outside {
            if watch == Watch::Auto {
                return Ok(false);
            }
            anyhow::bail!("attempted to watch path outside workspace, but it's prohibited in the current context");
        }
        Ok(true)
    }

    fn recorded(&self) -> std::sync::MutexGuard<'_, RecordedInputs> {
        self.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The path a value names: a string, relative to the base directory
    /// unless absolute; a `path`; or a label.
    pub fn path(&self, value: Value<'_>) -> anyhow::Result<PathBuf> {
        if let Some(path) = value.downcast_ref::<PathValue>() {
            return Ok(path.0.clone());
        }
        if let Some(label) = value.downcast_ref::<LabelValue>() {
            return self.graph.label_path(label.0);
        }
        let Some(text) = value.unpack_str() else {
            anyhow::bail!("expected a path, string or Label, got {}", value.get_type());
        };
        let path = Path::new(text);
        let joined = if path.is_absolute() { path.to_path_buf() } else { self.base_dir.join(path) };
        Ok(normalize(&joined))
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        if program.contains('/') {
            let path = PathBuf::from(program);
            return path.is_file().then_some(path);
        }
        let search = self.env.get("PATH")?;
        std::env::split_paths(search).map(|dir| dir.join(program)).find(|candidate| is_executable(candidate))
    }
}

/// Remove whatever is at `path`, so a symlink can be made there, as
/// `local_repository` makes the repository's own directory one: a
/// directory and its contents, or a file or symlink, which isn't followed.
fn remove_existing(path: &Path) -> anyhow::Result<()> {
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    let removed = match metadata.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    };
    removed.map_err(|error| anyhow::anyhow!("failed to replace {}: {error}", path.display()))
}

/// A path with `.` and `..` resolved lexically, as Bazel resolves the
/// paths repository rules give.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn write_file(path: &Path, content: &[u8], executable: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(path);
    std::fs::write(path, content).map_err(|error| anyhow::anyhow!("failed to write {}: {error}", path.display()))?;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// The URLs `url = ` gives: one string, or a list of them.
fn urls(value: Value<'_>) -> anyhow::Result<Vec<String>> {
    if let Some(url) = value.unpack_str() {
        return Ok(vec![url.to_string()]);
    }
    bazel_starlark::convert::strings(value).ok_or_else(|| anyhow::anyhow!("url must be a string or a list of strings"))
}

/// The URLs a download may use, as Bazel decides: each must be `http`,
/// `https` or `file`, and without a checksum to check what's downloaded,
/// plain `http` ones are left out.
fn usable_urls(urls: Vec<String>, checksum_given: bool) -> anyhow::Result<Vec<String>> {
    if urls.is_empty() {
        anyhow::bail!("urls not set");
    }
    let mut usable = Vec::with_capacity(urls.len());
    for url in urls {
        let scheme = url.split_once(':').map(|(scheme, _)| scheme.to_ascii_lowercase()).unwrap_or_default();
        if !matches!(scheme.as_str(), "http" | "https" | "file") {
            anyhow::bail!("Unsupported protocol: {scheme}");
        }
        if checksum_given || scheme != "http" {
            usable.push(url);
        }
    }
    if usable.is_empty() {
        anyhow::bail!("No URLs left after removing plain http URLs due to missing checksum. Please provide either a checksum or an https download location.");
    }
    Ok(usable)
}

fn integrity(sha256: &str, integrity: &str) -> anyhow::Result<Option<Integrity>> {
    if !integrity.is_empty() {
        return Ok(Some(Integrity::sri(integrity)?));
    }
    if !sha256.is_empty() {
        return Ok(Some(Integrity::sha256_hex(sha256)?));
    }
    Ok(None)
}

/// What `download` returns: a struct of whether it succeeded and the
/// file's checksums, or with `block = False`, a token whose `wait()`
/// returns that struct. bb downloads before returning either.
fn download_result<'v>(heap: Heap<'v>, success: bool, checksum: Option<&Integrity>, block: bool) -> Value<'v> {
    let result = DownloadToken {
        success,
        sha256: checksum.and_then(Integrity::sha256).unwrap_or_default(),
        integrity: checksum.map(Integrity::to_sri).unwrap_or_default(),
    };
    match block {
        true => result.to_struct(heap),
        false => heap.alloc(result),
    }
}

/// A download that `block = False` asked not to wait for.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct DownloadToken {
    success: bool,
    sha256: String,
    integrity: String,
}

starlark_simple_value!(DownloadToken);

impl DownloadToken {
    fn to_struct<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        heap.alloc(AllocStruct([
            ("success", heap.alloc(self.success)),
            ("sha256", heap.alloc(self.sha256.as_str())),
            ("integrity", heap.alloc(self.integrity.as_str())),
        ]))
    }
}

impl fmt::Display for DownloadToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<pending download>")
    }
}

#[starlark_value(type = "repository_ctx.download_token")]
impl<'v> StarlarkValue<'v> for DownloadToken {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("download_token", download_token_methods);
        Some(METHODS.methods())
    }
}

#[starlark_module]
fn download_token_methods(builder: &mut MethodsBuilder) {
    /// The download's result.
    fn wait<'v>(this: &DownloadToken, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(this.to_struct(heap))
    }
}

/// The arguments of `download` and `download_and_extract` that bb reads
/// from their keywords: `auth`, `headers` and `block`, with
/// `canonical_id`, which only names the download in Bazel's cache.
struct DownloadOptions {
    headers: RequestHeaders,
    block: bool,
    /// For `download_and_extract`, the leading components to remove from
    /// each entry's path.
    strip_components: usize,
}

impl DownloadOptions {
    fn of(function: &str, kwargs: &SmallMap<StringValue<'_>, Value<'_>>) -> anyhow::Result<DownloadOptions> {
        let mut options = DownloadOptions {
            headers: RequestHeaders::default(),
            block: true,
            strip_components: 0,
        };
        for (name, value) in kwargs.iter() {
            match name.as_str() {
                "canonical_id" => {}
                "auth" => options.headers.authorization = authorizations(*value)?,
                "headers" => options.headers.headers = headers(*value)?,
                "block" => options.block = value.unpack_bool().ok_or_else(|| anyhow::anyhow!("block must be a bool"))?,
                "strip_components" if function == "download_and_extract" => {
                    let count = value.unpack_i32().ok_or_else(|| anyhow::anyhow!("strip_components must be an int"))?;
                    options.strip_components = usize::try_from(count).map_err(|_| anyhow::anyhow!("'strip_components' must be non-negative"))?;
                }
                other => anyhow::bail!("{function}() got an unexpected keyword argument '{other}'"),
            }
        }
        Ok(options)
    }
}

/// The `Authorization` headers an `auth` dict gives, by URL: for each, a
/// dict of a `type`, `basic` or `pattern`, a `login` and `password`, and
/// for patterns, a `pattern` they're put in as `<login>` and `<password>`.
fn authorizations(auth: Value<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let dict = DictRef::from_value(auth).ok_or_else(|| anyhow::anyhow!("auth must be a dict"))?;
    let mut headers = Vec::new();
    for (url, entry) in dict.iter() {
        let url = url.unpack_str().ok_or_else(|| anyhow::anyhow!("auth's keys must be URLs"))?;
        let entry = DictRef::from_value(entry).ok_or_else(|| anyhow::anyhow!("auth's values must be dicts"))?;
        let field = |name: &str| entry.iter().find(|(key, _)| key.unpack_str() == Some(name)).and_then(|(_, value)| value.unpack_str().map(str::to_string));
        let login = field("login").unwrap_or_default();
        let password = field("password").unwrap_or_default();
        let header = match field("type").as_deref() {
            Some("basic") => bazel_fetch::Credentials { login, password }.basic_authorization(),
            Some("pattern") => field("pattern").unwrap_or_default().replace("<login>", &login).replace("<password>", &password),
            other => anyhow::bail!("auth for {url} has an unknown type: {other:?}"),
        };
        headers.push((url.to_string(), header));
    }
    Ok(headers)
}

/// The headers a `headers` dict gives: each a string, or a list of them.
fn headers(value: Value<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let dict = DictRef::from_value(value).ok_or_else(|| anyhow::anyhow!("headers must be a dict"))?;
    let mut headers = Vec::new();
    for (name, values) in dict.iter() {
        let name = name.unpack_str().ok_or_else(|| anyhow::anyhow!("headers' keys must be strings"))?;
        let values = match values.unpack_str() {
            Some(value) => vec![value.to_string()],
            None => bazel_starlark::convert::strings(values).ok_or_else(|| anyhow::anyhow!("headers' values must be strings or lists of them"))?,
        };
        headers.extend(values.into_iter().map(|value| (name.to_string(), value)));
    }
    Ok(headers)
}

/// An `output` argument, unless it's missing, `None` or empty.
fn given_output(output: Option<Value<'_>>) -> Option<Value<'_>> {
    output.filter(|output| !output.is_none() && output.unpack_str() != Some(""))
}

fn renames(value: Option<Value<'_>>) -> HashMap<String, String> {
    let Some(dict) = value.and_then(DictRef::from_value) else {
        return HashMap::new();
    };
    dict.iter()
        .filter_map(|(k, v)| Some((k.unpack_str()?.to_string(), v.unpack_str()?.to_string())))
        .collect()
}

/// Run a program, waiting at most `timeout`, and capture its output.
fn run(command: &mut Command, timeout: Duration) -> anyhow::Result<(i32, String, String)> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_thread = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();
    match status {
        Some(status) => Ok((status.code().unwrap_or(256 + status_signal(&status)), stdout, stderr)),
        None => Ok((256 + 14, stdout, format!("{stderr}Timed out"))),
    }
}

fn status_signal(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(0)
}

/// This machine's OS, as `repository_ctx.os` describes it.
pub fn os_struct<'v>(heap: Heap<'v>, env: &HashMap<String, String>) -> Value<'v> {
    let name = match std::env::consts::OS {
        "macos" => "mac os x",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        other => other,
    };
    let mut environ: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    environ.sort();
    heap.alloc(AllocStruct([
        ("name", heap.alloc(name)),
        ("arch", heap.alloc(arch)),
        ("environ", heap.alloc(AllocDict(environ))),
    ]))
}

#[starlark_module]
pub fn io_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn os<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(os_struct(heap, &io_of(this)?.env))
    }

    fn path<'v>(this: Value<'v>, path: Value<'v>) -> anyhow::Result<PathValue> {
        Ok(PathValue(io_of(this)?.path(path)?))
    }

    fn read<'v>(this: Value<'v>, path: Value<'v>, #[starlark(require = named)] watch: Option<&str>) -> anyhow::Result<String> {
        let io = io_of(this)?;
        let path = io.path(path)?;
        io.watch_file(&path, Watch::parse(watch)?)?;
        if path.is_dir() {
            anyhow::bail!("attempting to read() a directory: {}", path.display());
        }
        std::fs::read_to_string(&path).map_err(|error| anyhow::anyhow!("failed to read {}: {error}", path.display()))
    }

    fn file<'v>(
        this: Value<'v>,
        path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(default = true)] executable: bool,
        #[starlark(default = true)] legacy_utf8: bool,
    ) -> anyhow::Result<NoneType> {
        let _ = legacy_utf8;
        let path = io_of(this)?.path(path)?;
        write_file(&path, content.as_bytes(), executable)?;
        Ok(NoneType)
    }

    fn template<'v>(
        this: Value<'v>,
        path: Value<'v>,
        template: Value<'v>,
        substitutions: Option<Value<'v>>,
        #[starlark(default = true)] executable: bool,
        #[starlark(require = named)] watch_template: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        let template = io.path(template)?;
        io.watch_file(&template, Watch::parse(watch_template)?)?;
        let mut text = std::fs::read_to_string(&template).map_err(|error| anyhow::anyhow!("failed to read template {}: {error}", template.display()))?;
        if let Some(substitutions) = substitutions.and_then(DictRef::from_value) {
            for (from, to) in substitutions.iter() {
                if let (Some(from), Some(to)) = (from.unpack_str(), to.unpack_str()) {
                    text = text.replace(from, to);
                }
            }
        }
        write_file(&io.path(path)?, text.as_bytes(), executable)?;
        Ok(NoneType)
    }

    fn symlink<'v>(this: Value<'v>, target: Value<'v>, link_name: Value<'v>) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        let target = io.path(target)?;
        let link = io.path(link_name)?;
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        remove_existing(&link)?;
        std::os::unix::fs::symlink(&target, &link).map_err(|error| anyhow::anyhow!("failed to link {}: {error}", link.display()))?;
        Ok(NoneType)
    }

    fn delete<'v>(this: Value<'v>, path: Value<'v>) -> anyhow::Result<bool> {
        let path = io_of(this)?.path(path)?;
        let existed = path.symlink_metadata().is_ok();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else if existed {
            std::fs::remove_file(&path)?;
        }
        Ok(existed)
    }

    fn which<'v>(this: Value<'v>, program: &str) -> anyhow::Result<NoneOr<PathValue>> {
        Ok(match io_of(this)?.which(program) {
            Some(path) => NoneOr::Other(PathValue(path)),
            None => NoneOr::None,
        })
    }

    fn getenv<'v>(this: Value<'v>, name: &str, default: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(match io_of(this)?.getenv(name)? {
            Some(value) => heap.alloc(value),
            None => default.unwrap_or_else(Value::new_none),
        })
    }

    fn execute<'v>(
        this: Value<'v>,
        arguments: Value<'v>,
        #[starlark(default = 600)] timeout: i32,
        environment: Option<Value<'v>>,
        #[starlark(default = true)] quiet: bool,
        #[starlark(default = "")] working_directory: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = quiet;
        let io = io_of(this)?;
        let Some(items) = bazel_starlark::convert::items(arguments) else {
            anyhow::bail!("execute(): arguments must be a list");
        };
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            let text = match item.unpack_str() {
                Some(text) => text.to_string(),
                None => io.path(item)?.display().to_string(),
            };
            argv.push(text);
        }
        let Some((program, args)) = argv.split_first() else {
            anyhow::bail!("execute(): arguments must not be empty");
        };
        let program = if program.contains('/') { PathBuf::from(program) } else { io.which(program).unwrap_or_else(|| PathBuf::from(program)) };
        let dir = if working_directory.is_empty() { io.base_dir.clone() } else { io.base_dir.join(working_directory) };
        std::fs::create_dir_all(&dir)?;
        let mut command = Command::new(&program);
        command.args(args).current_dir(&dir).env_clear().envs(&io.env);
        if let Some(environment) = environment.and_then(DictRef::from_value) {
            for (name, value) in environment.iter() {
                let Some(name) = name.unpack_str() else {
                    continue;
                };
                match value.unpack_str() {
                    Some(value) => command.env(name, value),
                    None => command.env_remove(name),
                };
            }
        }
        let _span = tracing::info_span!("repository.execute", program = %program.display()).entered();
        let (code, stdout, stderr) = match run(&mut command, Duration::from_secs(timeout.max(1) as u64)) {
            Ok(result) => result,
            Err(error) => (256, String::new(), format!("failed to run {}: {error}", program.display())),
        };
        Ok(heap.alloc(AllocStruct([
            ("return_code", heap.alloc(code)),
            ("stdout", heap.alloc(stdout.as_str())),
            ("stderr", heap.alloc(stderr.as_str())),
        ])))
    }

    fn download<'v>(
        this: Value<'v>,
        url: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let options = DownloadOptions::of("download", &kwargs)?;
        let io = io_of(this)?;
        let dest = match given_output(output) {
            Some(output) => io.path(output)?,
            None => {
                let url = urls(url)?.first().cloned().unwrap_or_default();
                io.base_dir.join(url.rsplit('/').next().unwrap_or("download"))
            }
        };
        let expected = self::integrity(sha256, integrity)?;
        let urls = usable_urls(urls(url)?, expected.is_some())?;
        match io.downloads.download_with(&urls, expected.as_ref(), &dest, &options.headers) {
            Ok(checksum) => {
                if executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                }
                Ok(download_result(heap, true, Some(&checksum), options.block))
            }
            Err(_) if allow_fail => Ok(download_result(heap, false, None, options.block)),
            Err(error) => Err(error.into()),
        }
    }

    fn download_and_extract<'v>(
        this: Value<'v>,
        url: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = "")] r#type: &str,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] integrity: &str,
        #[allow(non_snake_case)]
        #[starlark(require = named)]
        stripPrefix: Option<&str>,
        #[starlark(require = named)] rename_files: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let options = DownloadOptions::of("download_and_extract", &kwargs)?;
        let io = io_of(this)?;
        let dest = match given_output(output) {
            Some(output) => io.path(output)?,
            None => io.base_dir.clone(),
        };
        let urls = urls(url)?;
        let archive_type = if r#type.is_empty() { ArchiveType::parse(urls.first().map(String::as_str).unwrap_or("")) } else { ArchiveType::parse(r#type) };
        let archive_type = archive_type?;
        let download = dest.join(format!(".bb_archive_{}", std::process::id()));
        let expected = self::integrity(sha256, integrity)?;
        let urls = usable_urls(urls, expected.is_some())?;
        let checksum = match io.downloads.download_with(&urls, expected.as_ref(), &download, &options.headers) {
            Ok(checksum) => checksum,
            Err(_) if allow_fail => return Ok(download_result(heap, false, None, options.block)),
            Err(error) => return Err(error.into()),
        };
        let prefix = stripPrefix.unwrap_or(strip_prefix);
        let extracted = bazel_fetch::extract(&download, archive_type, &dest, prefix, options.strip_components, &renames(rename_files));
        let _ = std::fs::remove_file(&download);
        extracted?;
        Ok(download_result(heap, true, Some(&checksum), options.block))
    }

    fn extract<'v>(
        this: Value<'v>,
        archive: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(require = named)] rename_files: Option<Value<'v>>,
        #[starlark(require = named)] watch_archive: Option<&str>,
        #[starlark(require = named, default = "")] r#type: &str,
        #[allow(non_snake_case)]
        #[starlark(require = named)]
        stripPrefix: Option<&str>,
        #[starlark(require = named, default = 0)] strip_components: i32,
    ) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        let archive = io.path(archive)?;
        io.watch_file(&archive, Watch::parse(watch_archive)?)?;
        let dest = match given_output(output) {
            Some(output) => io.path(output)?,
            None => io.base_dir.clone(),
        };
        let archive_type = if r#type.is_empty() { ArchiveType::parse(&archive.display().to_string()) } else { ArchiveType::parse(r#type) }?;
        let strip_components = usize::try_from(strip_components).map_err(|_| anyhow::anyhow!("'strip_components' must be non-negative"))?;
        bazel_fetch::extract(&archive, archive_type, &dest, stripPrefix.unwrap_or(strip_prefix), strip_components, &renames(rename_files))?;
        Ok(NoneType)
    }

    fn patch<'v>(
        this: Value<'v>,
        patch_file: Value<'v>,
        #[starlark(default = 0)] strip: i32,
        #[starlark(require = named)] directory: Option<Value<'v>>,
        #[starlark(require = named)] watch_patch: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        let patch = io.path(patch_file)?;
        io.watch_file(&patch, Watch::parse(watch_patch)?)?;
        let directory = match directory.filter(|directory| directory.unpack_str() != Some("")) {
            Some(directory) => io.path(directory)?,
            None => io.base_dir.clone(),
        };
        if !directory.starts_with(&io.base_dir) {
            anyhow::bail!("Cannot write outside of the repository directory for path {}", directory.display());
        }
        let text = std::fs::read_to_string(&patch).map_err(|error| anyhow::anyhow!("failed to read {}: {error}", patch.display()))?;
        bazel_fetch::apply_patch(&directory, &text, strip.max(0) as usize, &patch.display().to_string())?;
        Ok(NoneType)
    }

    fn report_progress<'v>(this: Value<'v>, #[starlark(default = "")] status: &str) -> anyhow::Result<NoneType> {
        let _ = this;
        if !status.is_empty() {
            tracing::debug!("{status}");
        }
        Ok(NoneType)
    }

    fn watch<'v>(this: Value<'v>, path: Value<'v>) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        io.watch_file(&io.path(path)?, Watch::Yes)?;
        Ok(NoneType)
    }

    fn watch_tree<'v>(this: Value<'v>, path: Value<'v>) -> anyhow::Result<NoneType> {
        let io = io_of(this)?;
        io.watch_tree(&io.path(path)?)?;
        Ok(NoneType)
    }
}
