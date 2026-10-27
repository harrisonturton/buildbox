//! Fetching repositories into the machine-wide repository directory.

use crate::config::BzlmodConfig;
use crate::repo_spec::{ArchiveSpec, GitSpec, RemoteFile, RepoSpec};
use crate::spec::RepoSpecKey;
use bazel_fetch::{ArchiveType, DownloadCache, Integrity};
use bazel_fs::{Root, RootedPath};
use bazel_label::{Label, LabelContext, PackageId, RepoMappingTable, RepoName};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The files that mark a directory as a repository's root. A fetched
/// repository without one gets an empty `REPO.bazel`, as Bazel gives it.
const BOUNDARY_FILES: [&str; 4] = ["REPO.bazel", "MODULE.bazel", "WORKSPACE", "WORKSPACE.bazel"];

/// A fetched repository's directory.
///
/// Fetched repositories don't change, so each is fetched once per machine,
/// into a directory named by its spec, and shared by every workspace whose
/// graph asks for the same spec, across server restarts too.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FetchedRepo {
    pub dir: Symbol,
}

encoded_value!(FetchedRepo);

impl FetchedRepo {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(self.dir.as_str())
    }
}

/// Fetches a repository by its canonical name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoFetchKey(pub RepoName);

impl Key for RepoFetchKey {
    type Value = FetchedRepo;
    const KIND: &'static str = "REPOSITORY_FETCH";

    async fn compute(&self, ctx: &Ctx) -> Result<FetchedRepo, SkyError> {
        let spec = ctx.compute(&RepoSpecKey(self.0)).await?;
        let fetched = ctx.compute(&FetchSpecKey { repo: self.0, spec: (*spec).clone() }).await?;
        Ok((*fetched).clone())
    }
}

/// Fetches a repository from a spec.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct FetchSpecKey {
    pub repo: RepoName,
    pub spec: RepoSpec,
}

impl Key for FetchSpecKey {
    type Value = FetchedRepo;
    const KIND: &'static str = "REPOSITORY_SPEC_FETCH";

    async fn compute(&self, ctx: &Ctx) -> Result<FetchedRepo, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        match &self.spec {
            RepoSpec::Builtin { dir } | RepoSpec::Local { path: dir } => Ok(FetchedRepo { dir: Symbol::intern(dir) }),
            RepoSpec::Archive(archive) => fetch_archive(ctx, &config, self.repo, archive).await,
            RepoSpec::Git(git) => fetch_git(ctx, &config, self.repo, git).await,
            RepoSpec::HostPlatform => host_platform(&config).await,
            RepoSpec::RepoRule(rule) => crate::repo_rule::fetch(ctx, self.repo, rule).await,
        }
    }
}

/// The directory a repository with this spec is fetched to. `spec_bytes`
/// must identify the spec the same way in every process, so it can't hold
/// interned ids: use strings.
pub fn repo_dir(config: &BzlmodConfig, repo: RepoName, spec_bytes: &[u8]) -> PathBuf {
    let digest = Fingerprint::of(spec_bytes).to_string();
    config.repos_dir.join(format!("{}-{}", repo, &digest[..16]))
}

/// The file beside a repository's directory that marks it completely
/// fetched. It's outside the directory so the repository's own globs don't
/// see it.
fn complete_marker(dest: &Path) -> PathBuf {
    PathBuf::from(format!("{}.fetched", dest.display()))
}

/// Run `fetch` to fill a repository's directory, unless the repository
/// was fetched before. Repositories are fetched in place, as in Bazel,
/// since repository rules may record the paths of the files they create; the
/// marker beside the directory says the fetch finished. A fetch that fails
/// is tried again by the next command, as Bazel's are. Blocks.
pub fn fetch_into(dest: &Path, fetch: impl FnOnce(&Path) -> Result<(), String>) -> Result<(), SkyError> {
    let marker = complete_marker(dest);
    if marker.exists() {
        link_external(dest);
        return Ok(());
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
    std::fs::create_dir_all(dest).map_err(|error| SkyError::transient(format!("failed to create {}: {error}", dest.display())))?;
    if let Err(message) = fetch(dest) {
        let _ = std::fs::remove_dir_all(dest);
        return Err(SkyError::transient(message));
    }
    if !BOUNDARY_FILES.iter().any(|name| dest.join(name).exists()) {
        let _ = std::fs::write(dest.join("REPO.bazel"), b"");
    }
    std::fs::write(&marker, b"").map_err(|error| SkyError::transient(format!("failed to write {}: {error}", marker.display())))?;
    link_external(dest);
    Ok(())
}

/// Link a fetched repository as `external/<canonical name>` beside the
/// repositories' directory, where Bazel would put it, for repository rules
/// that find other repositories by such paths.
fn link_external(dest: &Path) {
    let (Some(repos), Some(name)) = (dest.parent(), dest.file_name().and_then(|name| name.to_str())) else {
        return;
    };
    let Some((canonical, _)) = name.rsplit_once('-') else {
        return;
    };
    let Some(base) = repos.parent() else {
        return;
    };
    let external = base.join("external");
    let link = external.join(canonical);
    if std::fs::read_link(&link).is_ok_and(|target| target == dest) {
        return;
    }
    let _ = std::fs::create_dir_all(&external);
    let _ = std::fs::remove_file(&link);
    let _ = std::os::unix::fs::symlink(dest, &link);
}

async fn fetch_archive(ctx: &Ctx, config: &BzlmodConfig, repo: RepoName, archive: &ArchiveSpec) -> Result<FetchedRepo, SkyError> {
    let dest = repo_dir(config, repo, format!("{archive:?}").as_bytes());
    let label_patches = read_label_patches(ctx, &archive.local_patches).await?;
    let module_file = match &archive.module_file {
        Some(url) => ctx.compute(&crate::registry::RegistryFileKey(url.clone())).await?.0.clone(),
        None => None,
    };
    let downloads = config.downloads_for(ctx)?;
    let archive = archive.clone();
    let span = tracing::info_span!("bzlmod.fetch", repo = %repo);
    let fetched_dest = dest.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        fetch_into(&fetched_dest, |dir| fill_archive(dir, &downloads, &archive, &label_patches, module_file.as_deref()))
    })
    .await;
    result.map_err(|error| SkyError::transient(error.to_string()))??;
    Ok(FetchedRepo {
        dir: Symbol::intern(&dest.display().to_string()),
    })
}

async fn fetch_git(ctx: &Ctx, config: &BzlmodConfig, repo: RepoName, git: &GitSpec) -> Result<FetchedRepo, SkyError> {
    let dest = repo_dir(config, repo, format!("{git:?}").as_bytes());
    let label_patches = read_label_patches(ctx, &git.source.patches).await?;
    let module_file = match &git.module_file {
        Some(url) => ctx.compute(&crate::registry::RegistryFileKey(url.clone())).await?.0.clone(),
        None => None,
    };
    let downloads = config.downloads_for(ctx)?;
    let git = git.clone();
    let span = tracing::info_span!("bzlmod.git", repo = %repo);
    let fetched_dest = dest.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        fetch_into(&fetched_dest, |dir| fill_git(dir, repo, &downloads, &git, &label_patches, module_file.as_deref()))
    })
    .await;
    result.map_err(|error| SkyError::transient(error.to_string()))??;
    Ok(FetchedRepo {
        dir: Symbol::intern(&dest.display().to_string()),
    })
}

/// Fill `dir` with a Git repository, in `git_repository`'s order: the
/// checkout, the registry's patches, the registry's MODULE.bazel, the
/// override's patches and commands, then the registry's MODULE.bazel
/// again, which Bazel downloads after patching too.
fn fill_git(dir: &Path, repo: RepoName, downloads: &DownloadCache, git: &GitSpec, label_patches: &[Vec<u8>], module_file: Option<&[u8]>) -> Result<(), String> {
    let source = &git.source;
    let checkout = match source.strip_prefix.is_empty() {
        true => dir.to_path_buf(),
        false => dir.join(".tmp_git_root"),
    };
    crate::git::checkout(&checkout, repo.as_str(), source)?;
    if !source.strip_prefix.is_empty() {
        move_up(&checkout.join(&source.strip_prefix), dir, &source.strip_prefix)?;
        let _ = std::fs::remove_dir_all(&checkout);
    }
    for (index, patch) in git.registry_patches.iter().enumerate() {
        let contents = download_patch(dir, downloads, patch, index)?;
        apply(dir, &contents, git.registry_patch_strip, &patch.url)?;
    }
    if let Some(module_file) = module_file {
        write_module_file(dir, module_file)?;
    }
    for (label, contents) in source.patches.iter().zip(label_patches) {
        apply(dir, contents, source.patch_strip, label)?;
    }
    for command in &source.patch_cmds {
        run_patch_command(dir, command)?;
    }
    if let Some(module_file) = module_file {
        write_module_file(dir, module_file)?;
    }
    let _ = std::fs::remove_dir_all(dir.join(".git"));
    Ok(())
}

/// Move what's in `from`, a directory of the checkout, up to `to`, the
/// repository's root, as `strip_prefix` asks.
fn move_up(from: &Path, to: &Path, prefix: &str) -> Result<(), String> {
    let entries = std::fs::read_dir(from).map_err(|_| format!("strip_prefix at {prefix} does not exist in repo"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to list {}: {error}", from.display()))?;
        let target = to.join(entry.file_name());
        std::fs::rename(entry.path(), &target).map_err(|error| format!("failed to move {}: {error}", entry.path().display()))?;
    }
    Ok(())
}

/// The contents of patches given as labels of the main repository, in
/// order, read through the graph so the fetch depends on them.
async fn read_label_patches(ctx: &Ctx, labels: &[String]) -> Result<Vec<Vec<u8>>, SkyError> {
    let mut patches = Vec::with_capacity(labels.len());
    for label in labels {
        patches.push(read_label_patch(ctx, label).await?);
    }
    Ok(patches)
}

/// The contents of the patch `label`, a file of the main repository, read
/// through the graph.
pub async fn read_label_patch(ctx: &Ctx, label: &str) -> Result<Vec<u8>, SkyError> {
    let mapping = RepoMappingTable::new(RepoName::MAIN, Vec::new());
    let context = LabelContext {
        package: PackageId::new(RepoName::MAIN, PathId::ROOT),
        mapping: &mapping,
        lenient: false,
    };
    let parsed = Label::parse(label, &context).map_err(|error| SkyError::new(error.to_string()))?;
    let path = parsed.package.path.join_path(PathId::parse(parsed.name.as_str()).map_err(|e| SkyError::new(e.to_string()))?);
    let Some(bytes) = bazel_fs::read_file(ctx, RootedPath::new(Root::Workspace, path)).await? else {
        return Err(SkyError::new(format!("patch {label} doesn't exist")));
    };
    Ok(bytes)
}

/// Fill `dir` with an archive's repository, in Bazel's order: the archive
/// and its overlay, the registry's patches, the registry's MODULE.bazel,
/// then the main repository's patches and the override's commands. A
/// registry's patches are written against the archive's MODULE.bazel, so
/// they come before it's replaced.
fn fill_archive(dir: &Path, downloads: &DownloadCache, archive: &ArchiveSpec, label_patches: &[Vec<u8>], module_file: Option<&[u8]>) -> Result<(), String> {
    let integrity = parse_integrity(&archive.integrity)?;
    let name = archive.urls.first().cloned().unwrap_or_default();
    let archive_type = if archive.archive_type.is_empty() { ArchiveType::parse(&name) } else { ArchiveType::parse(&archive.archive_type) };
    let archive_type = archive_type.map_err(|error| error.to_string())?;
    let download = dir.join(".bb_download");
    downloads.download(&archive.urls, integrity.as_ref(), &download).map_err(|error| error.to_string())?;
    bazel_fetch::extract(&download, archive_type, dir, &archive.strip_prefix, 0, &HashMap::new()).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_file(&download);

    for (path, file) in &archive.overlay {
        let integrity = parse_integrity(&file.integrity)?;
        downloads
            .download(std::slice::from_ref(&file.url), integrity.as_ref(), &dir.join(path))
            .map_err(|error| error.to_string())?;
    }
    for (index, patch) in archive.patches.iter().enumerate() {
        let contents = download_patch(dir, downloads, patch, index)?;
        apply(dir, &contents, archive.patch_strip, &patch.url)?;
    }
    if let Some(module_file) = module_file {
        write_module_file(dir, module_file)?;
    }
    for (label, contents) in archive.local_patches.iter().zip(label_patches) {
        apply(dir, contents, archive.local_patch_strip, label)?;
    }
    for command in &archive.patch_cmds {
        run_patch_command(dir, command)?;
    }
    Ok(())
}

/// Run one of an override's `patch_cmds` in the repository, as Bazel does.
fn run_patch_command(dir: &Path, command: &str) -> Result<(), String> {
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("Error applying patch command {command}:\n{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("Error applying patch command {command}:\n{stdout}{stderr}"))
}

/// Download one of the registry's patches.
fn download_patch(dir: &Path, downloads: &DownloadCache, patch: &RemoteFile, index: usize) -> Result<Vec<u8>, String> {
    let file = dir.join(format!(".bb_patch_{index}"));
    let integrity = parse_integrity(&patch.integrity)?;
    downloads
        .download(std::slice::from_ref(&patch.url), integrity.as_ref(), &file)
        .map_err(|error| error.to_string())?;
    let contents = std::fs::read(&file).map_err(|error| format!("failed to read patch {}: {error}", file.display()))?;
    let _ = std::fs::remove_file(&file);
    Ok(contents)
}

/// Replace the archive's MODULE.bazel with the registry's.
fn write_module_file(dir: &Path, module_file: &[u8]) -> Result<(), String> {
    let path = dir.join("MODULE.bazel");
    make_writable(&path);
    std::fs::write(&path, module_file).map_err(|error| format!("failed to write {}: {error}", path.display()))
}

fn apply(dir: &Path, contents: &[u8], strip: i64, name: &str) -> Result<(), String> {
    let text = String::from_utf8_lossy(contents);
    bazel_fetch::apply_patch(dir, &text, strip as usize, name).map_err(|error| error.to_string())
}

/// Archives can contain read-only files that the registry's files replace.
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o200);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

fn parse_integrity(text: &str) -> Result<Option<Integrity>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    let parsed = if text.contains('-') { Integrity::sri(text) } else { Integrity::sha256_hex(text) };
    parsed.map(Some).map_err(|error| error.to_string())
}

/// `@local_config_platform`: a `host` platform with this machine's CPU and
/// operating system.
async fn host_platform(config: &BzlmodConfig) -> Result<FetchedRepo, SkyError> {
    let constraints = format!("HOST_CONSTRAINTS = [\"@platforms//cpu:{}\", \"@platforms//os:{}\"]\n", host_cpu(), host_os());
    let dest = repo_dir(config, RepoName::new("local_config_platform"), constraints.as_bytes());
    let fetched_dest = dest.clone();
    let result = tokio::task::spawn_blocking(move || {
        fetch_into(&fetched_dest, |dir| {
            let build = "load(\":constraints.bzl\", \"HOST_CONSTRAINTS\")\n\npackage(default_visibility = [\"//visibility:public\"])\n\nplatform(\n    name = \"host\",\n    constraint_values = HOST_CONSTRAINTS,\n)\n\nexports_files([\"constraints.bzl\"])\n";
            std::fs::write(dir.join("BUILD.bazel"), build).map_err(|error| error.to_string())?;
            std::fs::write(dir.join("constraints.bzl"), &constraints).map_err(|error| error.to_string())?;
            std::fs::write(dir.join("MODULE.bazel"), "module(name = \"local_config_platform\")\n").map_err(|error| error.to_string())
        })
    })
    .await;
    result.map_err(|error| SkyError::transient(error.to_string()))??;
    Ok(FetchedRepo {
        dir: Symbol::intern(&dest.display().to_string()),
    })
}

/// This machine's CPU, as `@platforms//cpu` names it.
pub fn host_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" => "arm",
        "riscv64" => "riscv64",
        "powerpc64" => "ppc",
        "s390x" => "s390x",
        other => other,
    }
}

/// This machine's operating system, as `@platforms//os` names it.
pub fn host_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        "freebsd" => "freebsd",
        other => other,
    }
}
