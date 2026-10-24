//! What repository rules and module extensions can do to the machine:
//! read and write files, download and extract archives, and run programs.
//! `repository_ctx` and `module_ctx` share these methods.

use crate::graph_access::GraphAccess;
use crate::path_value::PathValue;
use bazel_fetch::{ArchiveType, DownloadCache, Integrity};
use bazel_starlark::label_value::LabelValue;
use starlark::collections::SmallMap;
use starlark::environment::MethodsBuilder;
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::none::{NoneOr, NoneType};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, StringValue, Value, ValueLike};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a context's I/O methods work with.
pub struct IoState {
    /// The directory relative paths are relative to: the repository being
    /// fetched, or a module extension's working directory.
    pub base_dir: PathBuf,
    pub graph: GraphAccess,
    pub downloads: DownloadCache,
    pub env: HashMap<String, String>,
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

fn integrity(sha256: &str, integrity: &str) -> anyhow::Result<Option<Integrity>> {
    if !integrity.is_empty() {
        return Ok(Some(Integrity::sri(integrity)?));
    }
    if !sha256.is_empty() {
        return Ok(Some(Integrity::sha256_hex(sha256)?));
    }
    Ok(None)
}

fn download_result<'v>(heap: Heap<'v>, success: bool, checksum: Option<&Integrity>) -> Value<'v> {
    let sha256 = checksum.and_then(Integrity::sha256).unwrap_or_default();
    let sri = checksum.map(Integrity::to_sri).unwrap_or_default();
    heap.alloc(AllocStruct([
        ("success", heap.alloc(success)),
        ("sha256", heap.alloc(sha256.as_str())),
        ("integrity", heap.alloc(sri.as_str())),
    ]))
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

    fn read<'v>(this: Value<'v>, path: Value<'v>, #[starlark(require = named)] watch: Option<Value<'v>>) -> anyhow::Result<String> {
        let _ = watch;
        let path = io_of(this)?.path(path)?;
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
        #[starlark(require = named)] watch_template: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = watch_template;
        let io = io_of(this)?;
        let template = io.path(template)?;
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
        let _ = std::fs::remove_file(&link);
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
        Ok(match io_of(this)?.env.get(name) {
            Some(value) => heap.alloc(value.as_str()),
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
        let _ = kwargs;
        let io = io_of(this)?;
        let dest = match given_output(output) {
            Some(output) => io.path(output)?,
            None => {
                let url = urls(url)?.first().cloned().unwrap_or_default();
                io.base_dir.join(url.rsplit('/').next().unwrap_or("download"))
            }
        };
        let expected = self::integrity(sha256, integrity)?;
        match io.downloads.download(&urls(url)?, expected.as_ref(), &dest) {
            Ok(checksum) => {
                if executable {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                }
                Ok(download_result(heap, true, Some(&checksum)))
            }
            Err(_) if allow_fail => Ok(download_result(heap, false, None)),
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
        let _ = kwargs;
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
        let checksum = match io.downloads.download(&urls, expected.as_ref(), &download) {
            Ok(checksum) => checksum,
            Err(_) if allow_fail => return Ok(download_result(heap, false, None)),
            Err(error) => return Err(error.into()),
        };
        let prefix = stripPrefix.unwrap_or(strip_prefix);
        let extracted = bazel_fetch::extract(&download, archive_type, &dest, prefix, &renames(rename_files));
        let _ = std::fs::remove_file(&download);
        extracted?;
        Ok(download_result(heap, true, Some(&checksum)))
    }

    fn extract<'v>(
        this: Value<'v>,
        archive: Value<'v>,
        output: Option<Value<'v>>,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(require = named)] rename_files: Option<Value<'v>>,
        #[starlark(require = named)] watch_archive: Option<Value<'v>>,
        #[starlark(require = named, default = "")] r#type: &str,
        #[allow(non_snake_case)]
        #[starlark(require = named)]
        stripPrefix: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let _ = watch_archive;
        let io = io_of(this)?;
        let archive = io.path(archive)?;
        let dest = match given_output(output) {
            Some(output) => io.path(output)?,
            None => io.base_dir.clone(),
        };
        let archive_type = if r#type.is_empty() { ArchiveType::parse(&archive.display().to_string()) } else { ArchiveType::parse(r#type) }?;
        bazel_fetch::extract(&archive, archive_type, &dest, stripPrefix.unwrap_or(strip_prefix), &renames(rename_files))?;
        Ok(NoneType)
    }

    fn patch<'v>(
        this: Value<'v>,
        patch_file: Value<'v>,
        #[starlark(default = 0)] strip: i32,
        #[starlark(require = named)] directory: Option<Value<'v>>,
        #[starlark(require = named)] watch_patch: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = watch_patch;
        let io = io_of(this)?;
        let patch = io.path(patch_file)?;
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
        let _ = (this, path);
        Ok(NoneType)
    }

    fn watch_tree<'v>(this: Value<'v>, path: Value<'v>) -> anyhow::Result<NoneType> {
        let _ = (this, path);
        Ok(NoneType)
    }
}
