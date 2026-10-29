//! Reading modules from registries.

use crate::config::BzlmodConfig;
use crate::lockfile::{LockfileKey, LockfileMode, LockfileModeKey};
use crate::module_file::{self, GitSource, ModuleFile};
use crate::repo_spec::{ArchiveSpec, GitSpec, RemoteFile, RepoSpec};
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A graph service: the registries the latest command gave.
#[derive(Debug, Default)]
pub struct CommandRegistries(Mutex<Registries>);

impl CommandRegistries {
    pub fn new() -> Arc<CommandRegistries> {
        Arc::default()
    }

    /// Set the registries, returning whether they changed, in which case
    /// [`RegistriesKey`] must be invalidated.
    pub fn set(&self, registries: Registries) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != registries;
        *current = registries;
        changed
    }
}

/// The registries to resolve modules from, in order, as `--registry`
/// gives them, and `--module_mirrors`, the mirrors of their archives.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Registries {
    pub registries: Vec<String>,
    pub mirrors: Vec<String>,
}

encoded_value!(Registries);

/// The registries the latest command gave, or the Bazel Central Registry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegistriesKey;

impl Key for RegistriesKey {
    type Value = Registries;
    const KIND: &'static str = "REGISTRIES";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<Registries, SkyError> {
        let mut given = ctx.global::<CommandRegistries>()?.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
        if given.registries.is_empty() {
            given.registries.push(crate::config::BCR.to_string());
        }
        Ok(given)
    }
}

/// A file from a registry, or `None` if the registry doesn't have it.
///
/// Registry files never change once published, so they're cached on disk by
/// URL and shared by every workspace.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RegistryFile(pub Option<Vec<u8>>);

encoded_value!(RegistryFile);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegistryFileKey(pub String);

impl Key for RegistryFileKey {
    type Value = RegistryFile;
    const KIND: &'static str = "REGISTRY_FILE";

    async fn compute(&self, ctx: &Ctx) -> Result<RegistryFile, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        let url = self.0.clone();
        let cache_path = config.registry_cache.join(Fingerprint::of(url.as_bytes()).to_string());
        let downloads = config.downloads_for(ctx)?;
        let result = tokio::task::spawn_blocking(move || read_registry_file(&downloads, &url, &cache_path)).await;
        result.map_err(|error| SkyError::transient(error.to_string()))?
    }
}

/// The versions of a module a registry has yanked, with why, from its
/// `metadata.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct YankedVersions(pub Vec<(String, String)>);

encoded_value!(YankedVersions);

/// Reads the versions of a module a registry has yanked. Unlike the
/// registry's other files, `metadata.json` changes, so it isn't cached on
/// disk.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct YankedVersionsKey {
    pub registry: String,
    pub name: String,
}

impl Key for YankedVersionsKey {
    type Value = YankedVersions;
    const KIND: &'static str = "YANKED_VERSIONS";

    async fn compute(&self, ctx: &Ctx) -> Result<YankedVersions, SkyError> {
        let config = ctx.global::<BzlmodConfig>()?;
        let url = format!("{}/modules/{}/metadata.json", self.registry.trim_end_matches('/'), self.name);
        let downloads = config.downloads_for(ctx)?;
        let partial = config.registry_cache.join(format!("metadata.{}.{}.download", Fingerprint::of(url.as_bytes()), std::process::id()));
        let fetch_url = url.clone();
        let fetched = tokio::task::spawn_blocking(move || {
            let read = read_registry_file(&downloads, &fetch_url, &partial);
            let _ = std::fs::remove_file(&partial);
            read
        })
        .await;
        let file = fetched.map_err(|error| SkyError::transient(error.to_string()))??;
        let Some(bytes) = file.0 else {
            return Ok(YankedVersions::default());
        };
        let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| SkyError::new(format!("invalid {url}: {error}")))?;
        let Some(yanked) = json.get("yanked_versions").and_then(|yanked| yanked.as_object()) else {
            return Ok(YankedVersions::default());
        };
        let versions = yanked.iter().map(|(version, reason)| (version.clone(), reason.as_str().unwrap_or_default().to_string())).collect();
        Ok(YankedVersions(versions))
    }
}

/// A registry file, from the disk cache unless it's in a local registry,
/// whose files may change.
fn read_registry_file(downloads: &bazel_fetch::DownloadCache, url: &str, cache_path: &Path) -> Result<RegistryFile, SkyError> {
    if let Some(path) = url.strip_prefix("file://") {
        return match std::fs::read(path) {
            Ok(bytes) => Ok(RegistryFile(Some(bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RegistryFile(None)),
            Err(error) => Err(SkyError::new(format!("failed to read {path}: {error}"))),
        };
    }
    if let Ok(bytes) = std::fs::read(cache_path) {
        return Ok(RegistryFile(Some(bytes)));
    }
    let partial = cache_path.with_extension(format!("{}.download", std::process::id()));
    match downloads.download(&[url.to_string()], None, &partial) {
        Ok(_) => {}
        Err(bazel_fetch::DownloadError::NotFound(_)) => return Ok(RegistryFile(None)),
        Err(bazel_fetch::DownloadError::Http { message, .. }) => return Err(SkyError::transient(format!("Failed to fetch registry file {url}: {message}"))),
        Err(error) => return Err(SkyError::transient(error.to_string())),
    }
    let bytes = std::fs::read(&partial).map_err(|error| SkyError::transient(format!("failed to read {}: {error}", partial.display())))?;
    let _ = std::fs::rename(&partial, cache_path);
    Ok(RegistryFile(Some(bytes)))
}

pub fn module_file_url(registry: &str, name: &str, version: &str) -> String {
    format!("{}/modules/{name}/{version}/MODULE.bazel", registry.trim_end_matches('/'))
}

fn source_url(registry: &str, name: &str, version: &str) -> String {
    format!("{}/modules/{name}/{version}/source.json", registry.trim_end_matches('/'))
}

/// A module version from a registry.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RegistryModule {
    pub registry: String,
    pub file: ModuleFile,
    pub spec: RepoSpec,
    /// The URLs of the MODULE.bazel files looked for, in each registry
    /// until one had it, with their SHA-256, `None` where it was missing.
    pub module_files: Vec<(String, Option<String>)>,
    /// The other registry files the module's spec came from, with their
    /// SHA-256: its `source.json` and the registry's `bazel_registry.json`.
    pub source_files: Vec<(String, Option<String>)>,
}

/// A module version from the first registry that has it, or if none has
/// it, what each said, as lines of a message.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum RegistryLookup {
    Found(RegistryModule),
    Missing(String),
}

encoded_value!(RegistryLookup);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegistryModuleKey {
    pub registries: Vec<String>,
    /// `--module_mirrors`.
    pub mirrors: Vec<String>,
    pub name: String,
    pub version: String,
}

impl Key for RegistryModuleKey {
    type Value = RegistryLookup;
    const KIND: &'static str = "REGISTRY_MODULE";

    async fn compute(&self, ctx: &Ctx) -> Result<RegistryLookup, SkyError> {
        let mut missing = String::new();
        let mut module_files = Vec::new();
        let mode = ctx.compute(&LockfileModeKey).await?;
        let lockfile = ctx.compute(&LockfileKey).await?;
        for registry in &self.registries {
            let url = module_file_url(registry, &self.name, &self.version);
            let known_missing = lockfile.registry_file_hashes.get(&url).is_some_and(Option::is_none);
            if known_missing && matches!(*mode, LockfileMode::Update | LockfileMode::Error) {
                missing.push_str(&format!("\n* {url}: previously not found (as recorded in MODULE.bazel.lock, refresh with --lockfile_mode=refresh)"));
                module_files.push((url, None));
                continue;
            }
            let module_file = ctx.compute(&RegistryFileKey(url.clone())).await;
            let module_file = module_file.map_err(|error| error.context(format!("Error accessing registry {registry}")))?;
            let Some(bytes) = &module_file.0 else {
                missing.push_str(&format!("\n* {url}: not found"));
                module_files.push((url, None));
                continue;
            };
            module_files.push((url.clone(), Some(bazel_fetch::sha256_hex_of(bytes))));
            let source = String::from_utf8_lossy(bytes).into_owned();
            let file = module_file::evaluate(&url, &source, false, None).map_err(|error| SkyError::new(format!("{url}: {error}")))?;
            let source_json_url = source_url(registry, &self.name, &self.version);
            let source_json = ctx.compute(&RegistryFileKey(source_json_url.clone())).await?;
            let Some(source_json) = &source_json.0 else {
                return Err(SkyError::new(format!("{registry} has no source.json for {}@{}", self.name, self.version)));
            };
            let (registry_json, registry_json_file) = registry_json(ctx, registry).await?;
            let source_files = vec![(source_json_url, Some(bazel_fetch::sha256_hex_of(source_json))), registry_json_file];
            let mut mirrors = self.mirrors.clone();
            mirrors.extend(registry_json.mirrors.iter().filter(|mirror| !self.mirrors.contains(mirror)).cloned());
            let found = Found {
                registry,
                name: &self.name,
                version: &self.version,
                module_file: &url,
                mirrors: &mirrors,
                module_base_path: registry_json.module_base_path.as_deref(),
            };
            let spec = parse_source(&found, source_json)?;
            return Ok(RegistryLookup::Found(RegistryModule {
                registry: registry.clone(),
                file,
                spec,
                module_files,
                source_files,
            }));
        }
        Ok(RegistryLookup::Missing(missing))
    }
}

/// What a registry's `bazel_registry.json` says of it.
#[derive(Debug, Default)]
struct RegistryJson {
    mirrors: Vec<String>,
    module_base_path: Option<String>,
}

/// A registry's `bazel_registry.json`, or the defaults if it has none,
/// with its URL and SHA-256.
async fn registry_json(ctx: &Ctx, registry: &str) -> Result<(RegistryJson, (String, Option<String>)), SkyError> {
    let url = format!("{}/bazel_registry.json", registry.trim_end_matches('/'));
    let file = ctx.compute(&RegistryFileKey(url.clone())).await;
    let file = file.map_err(|error| error.context(format!("Error accessing registry {registry}")))?;
    let Some(bytes) = &file.0 else {
        return Ok((RegistryJson::default(), (url, None)));
    };
    let hash = bazel_fetch::sha256_hex_of(bytes);
    let json: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| SkyError::new(format!("invalid {url}: {error}")))?;
    let mirrors = match json.get("mirrors").and_then(|mirrors| mirrors.as_array()) {
        Some(mirrors) => mirrors.iter().filter_map(|mirror| mirror.as_str()).map(|mirror| mirror.trim_end_matches('/').to_string()).collect(),
        None => Vec::new(),
    };
    let module_base_path = json.get("module_base_path").and_then(|path| path.as_str()).map(str::to_string);
    Ok((RegistryJson { mirrors, module_base_path }, (url, Some(hash))))
}

/// A module version found in a registry, and what the registry says of
/// where its sources are.
struct Found<'a> {
    registry: &'a str,
    name: &'a str,
    version: &'a str,
    /// The URL of the module's MODULE.bazel in the registry.
    module_file: &'a str,
    /// The mirrors of archives: `--module_mirrors`, then the registry's.
    mirrors: &'a [String],
    /// The directory `local_path` sources are relative to.
    module_base_path: Option<&'a str>,
}

/// An archive's URLs, as Bazel lists them: through each mirror, then
/// itself, then the `mirror_urls` of `source.json`.
fn archive_urls(url: &str, mirrors: &[String], mirror_urls: &[String]) -> Vec<String> {
    let mut urls = Vec::new();
    let without_scheme = url.split_once("://").map(|(_, rest)| rest);
    for mirror in mirrors {
        if let Some(rest) = without_scheme {
            urls.push(format!("{}/{rest}", mirror.trim_end_matches('/')));
        }
    }
    urls.push(url.to_string());
    urls.extend(mirror_urls.iter().cloned());
    urls
}

/// The directory of a `local_path` source: its path, relative to the
/// registry's `module_base_path`, itself relative to a local registry.
fn local_path(found: &Found<'_>, path: &str) -> Result<String, String> {
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    let base = found.module_base_path.unwrap_or_default();
    if base.starts_with('/') {
        return Ok(format!("{base}/{path}"));
    }
    match found.registry.strip_prefix("file://") {
        Some(root) if root.starts_with('/') => Ok(format!("{}/{base}/{path}", root.trim_end_matches('/'))),
        Some(root) => Err(format!("Provided non absolute local registry path for module {}@{}: {root}", found.name, found.version)),
        None => Err(format!("Provided non local registry for module {}@{}", found.name, found.version)),
    }
}

/// Turn a registry's `source.json` into a repository spec.
fn parse_source(found: &Found<'_>, json: &[u8]) -> Result<RepoSpec, SkyError> {
    let Found { registry, name, version, module_file, .. } = *found;
    let invalid = |message: &str| SkyError::new(format!("invalid source.json for {name}@{version}: {message}"));
    let source: serde_json::Value = serde_json::from_slice(json).map_err(|error| invalid(&error.to_string()))?;
    let string = |field: &str| source.get(field).and_then(|value| value.as_str()).unwrap_or("").to_string();
    let base = format!("{}/modules/{name}/{version}", registry.trim_end_matches('/'));
    match source.get("type").and_then(|value| value.as_str()).unwrap_or("archive") {
        "archive" => {
            let url = string("url");
            if url.is_empty() {
                return Err(invalid("missing url"));
            }
            let mirror_urls: Vec<String> = match source.get("mirror_urls").and_then(|value| value.as_array()) {
                Some(mirrors) => mirrors.iter().filter_map(|mirror| mirror.as_str().map(str::to_string)).collect(),
                None => Vec::new(),
            };
            let urls = archive_urls(&url, found.mirrors, &mirror_urls);
            // Patches apply in the order source.json lists them.
            let entries = |field: &str, dir: &str| -> Vec<(String, RemoteFile)> {
                let Some(map) = source.get(field).and_then(|value| value.as_object()) else {
                    return Vec::new();
                };
                map.iter()
                    .map(|(file, integrity)| {
                        let remote = RemoteFile {
                            url: format!("{base}/{dir}/{file}"),
                            integrity: integrity.as_str().unwrap_or("").to_string(),
                        };
                        (file.clone(), remote)
                    })
                    .collect()
            };
            Ok(RepoSpec::Archive(ArchiveSpec {
                urls,
                integrity: string("integrity"),
                strip_prefix: string("strip_prefix"),
                archive_type: string("archive_type"),
                patches: entries("patches", "patches").into_iter().map(|(_, file)| file).collect(),
                patch_strip: source.get("patch_strip").and_then(|value| value.as_i64()).unwrap_or(0),
                local_patches: Vec::new(),
                local_patch_strip: 0,
                patch_cmds: Vec::new(),
                overlay: entries("overlay", "overlay"),
                module_file: Some(module_file.to_string()),
            }))
        }
        "local_path" => {
            let path = local_path(found, &string("path")).map_err(SkyError::new)?;
            Ok(RepoSpec::Local { path })
        }
        "git_repository" => {
            let flag = |field: &str| source.get(field).and_then(|value| value.as_bool()).unwrap_or(false);
            let git = GitSource {
                remote: string("remote"),
                commit: string("commit"),
                tag: string("tag"),
                shallow_since: string("shallow_since"),
                init_submodules: flag("init_submodules"),
                strip_prefix: string("strip_prefix"),
                ..GitSource::default()
            };
            let patches = match source.get("patches").and_then(|value| value.as_object()) {
                Some(patches) => patches
                    .iter()
                    .map(|(file, integrity)| RemoteFile {
                        url: format!("{base}/patches/{file}"),
                        integrity: integrity.as_str().unwrap_or("").to_string(),
                    })
                    .collect(),
                None => Vec::new(),
            };
            Ok(RepoSpec::Git(GitSpec {
                source: git,
                registry_patches: patches,
                registry_patch_strip: source.get("patch_strip").and_then(|value| value.as_i64()).unwrap_or(0),
                module_file: Some(module_file.to_string()),
            }))
        }
        other => Err(invalid(&format!("unsupported source type '{other}'"))),
    }
}
