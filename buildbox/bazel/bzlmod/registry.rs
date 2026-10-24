//! Reading modules from registries.

use crate::config::BzlmodConfig;
use crate::module_file::{self, ModuleFile};
use crate::repo_spec::{ArchiveSpec, RemoteFile, RepoSpec};
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::path::Path;

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
        let downloads = config.downloads.clone();
        let result = tokio::task::spawn_blocking(move || read_registry_file(&downloads, &url, &cache_path)).await;
        result.map_err(|error| SkyError::transient(error.to_string()))?
    }
}

fn read_registry_file(downloads: &bazel_fetch::DownloadCache, url: &str, cache_path: &Path) -> Result<RegistryFile, SkyError> {
    if let Ok(bytes) = std::fs::read(cache_path) {
        return Ok(RegistryFile(Some(bytes)));
    }
    let partial = cache_path.with_extension(format!("{}.download", std::process::id()));
    match downloads.download(&[url.to_string()], None, &partial) {
        Ok(_) => {}
        Err(bazel_fetch::DownloadError::NotFound(_)) => return Ok(RegistryFile(None)),
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

/// A module version from the first registry that has it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RegistryModule {
    pub registry: String,
    pub file: ModuleFile,
    pub spec: RepoSpec,
}

encoded_value!(RegistryModule);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegistryModuleKey {
    pub registries: Vec<String>,
    pub name: String,
    pub version: String,
}

impl Key for RegistryModuleKey {
    type Value = RegistryModule;
    const KIND: &'static str = "REGISTRY_MODULE";

    async fn compute(&self, ctx: &Ctx) -> Result<RegistryModule, SkyError> {
        for registry in &self.registries {
            let url = module_file_url(registry, &self.name, &self.version);
            let module_file = ctx.compute(&RegistryFileKey(url.clone())).await?;
            let Some(bytes) = &module_file.0 else {
                continue;
            };
            let source = String::from_utf8_lossy(bytes).into_owned();
            let file = module_file::evaluate(&url, &source, false, None).map_err(|error| SkyError::new(format!("{url}: {error}")))?;
            let source_json = ctx.compute(&RegistryFileKey(source_url(registry, &self.name, &self.version))).await?;
            let Some(source_json) = &source_json.0 else {
                return Err(SkyError::new(format!("{registry} has no source.json for {}@{}", self.name, self.version)));
            };
            let spec = parse_source(registry, &self.name, &self.version, source_json, &url)?;
            return Ok(RegistryModule {
                registry: registry.clone(),
                file,
                spec,
            });
        }
        Err(SkyError::new(format!(
            "module not found in registries: {}@{}",
            self.name, self.version
        )))
    }
}

/// Turn a registry's `source.json` into a repository spec.
fn parse_source(registry: &str, name: &str, version: &str, json: &[u8], module_file: &str) -> Result<RepoSpec, SkyError> {
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
            let mut urls = vec![url];
            if let Some(mirrors) = source.get("mirror_urls").and_then(|value| value.as_array()) {
                urls.extend(mirrors.iter().filter_map(|mirror| mirror.as_str().map(str::to_string)));
            }
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
                overlay: entries("overlay", "overlay"),
                module_file: Some(module_file.to_string()),
            }))
        }
        "local_path" => {
            let path = string("path");
            let local = registry.strip_prefix("file://").map(|root| Path::new(root).join(&path));
            match local {
                Some(path) => Ok(RepoSpec::Local {
                    path: path.display().to_string(),
                }),
                None => Err(invalid("local_path sources need a file:// registry")),
            }
        }
        other => Err(invalid(&format!("unsupported source type '{other}'"))),
    }
}
