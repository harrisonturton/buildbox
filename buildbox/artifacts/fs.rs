//! An [`ArtifactStore`] that keeps artifacts on the local filesystem.

mod unpack;

use artifacts::{ArtifactError, ArtifactInfo, ArtifactKind, ArtifactStore, ContainerImage, Usage};
use async_trait::async_trait;
use cas::{Cas, Digest};
use fsutil::run_blocking;
use oci::{Compression, ImageConfig, Manifest, OciError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

/// Stores artifacts as files.
///
/// The directory layout is:
///
/// - `lock`: held while the store is open, so only one process uses it.
/// - `refs/<name>`: a JSON record of each artifact: its kind, its digest and
///   when it was imported.
/// - `images/<hash>/`: unpacked container images, by the hash of their
///   manifest, so artifacts with the same contents share them. `rootfs/` is
///   the root filesystem and `image.json` describes it.
/// - `tmp/`: imports in progress, and images being deleted. Anything here
///   when the store is opened is deleted.
///
/// An image's files are deleted once no artifact refers to them and no
/// action is using them.
#[derive(Clone)]
pub struct FsArtifactStore {
    inner: Arc<Inner>,
}

struct Inner {
    cas: Arc<dyn Cas>,
    refs: PathBuf,
    images: PathBuf,
    tmp: PathBuf,

    /// How many leases each image has, by hash. Held while changing refs or
    /// images, so deciding whether an image is unused can't race with
    /// another request starting to use it.
    leases: Mutex<HashMap<String, usize>>,

    // Never read: holding the file holds the lock.
    _lock: File,
}

/// The contents of a file in `refs/`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    kind: RecordKind,
    hash: String,
    size_bytes: u64,
    imported_at_unix_millis: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordKind {
    ContainerImage,
}

/// The contents of `images/<hash>/image.json`.
#[derive(Debug, Serialize, Deserialize)]
struct ImageRecord {
    env: Vec<(String, String)>,
    size_bytes: u64,
}

const ROOTFS_DIR: &str = "rootfs";
const IMAGE_RECORD_FILE: &str = "image.json";

impl FsArtifactStore {
    /// Open the store in `dir`, creating it if needed. Artifacts are
    /// imported from blobs in `cas`.
    ///
    /// Fails if another process already has the store open.
    pub async fn open(dir: &Path, cas: Arc<dyn Cas>) -> Result<FsArtifactStore, ArtifactError> {
        let dir = dir.to_path_buf();
        run_blocking(move || open_blocking(&dir, cas)).await
    }
}

fn open_blocking(dir: &Path, cas: Arc<dyn Cas>) -> Result<FsArtifactStore, ArtifactError> {
    create_dir_all(dir)?;
    let lock = fsutil::lock_dir(dir).map_err(|error| io_error("failed to lock", dir, error))?;

    let refs = dir.join("refs");
    create_dir_all(&refs)?;
    let images = dir.join("images");
    create_dir_all(&images)?;
    let tmp = dir.join("tmp");
    fsutil::reset_dir(&tmp).map_err(|error| io_error("failed to reset", &tmp, error))?;

    let inner = Inner {
        cas,
        refs,
        images,
        tmp,
        leases: Mutex::new(HashMap::new()),
        _lock: lock,
    };
    remove_unreferenced_images(&inner)?;
    Ok(FsArtifactStore { inner: Arc::new(inner) })
}

/// Delete images no artifact refers to, e.g. after a crash midway through
/// replacing an artifact. Nothing is using them, since the store has only
/// just been opened.
fn remove_unreferenced_images(inner: &Inner) -> Result<(), ArtifactError> {
    let referenced = referenced_hashes(inner)?;
    let entries = std::fs::read_dir(&inner.images).map_err(|error| io_error("failed to list", &inner.images, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error("failed to list", &inner.images, error))?;
        let hash = entry.file_name().to_string_lossy().into_owned();
        if !referenced.contains(&hash) {
            tracing::info!("deleting unused container image {hash}");
            let path = entry.path();
            std::fs::remove_dir_all(&path).map_err(|error| io_error("failed to remove", &path, error))?;
        }
    }
    Ok(())
}

#[async_trait]
impl ArtifactStore for FsArtifactStore {
    #[tracing::instrument(skip_all, fields(name, manifest = %manifest))]
    async fn import_container_image(&self, name: &str, manifest: &Digest) -> Result<ArtifactInfo, ArtifactError> {
        artifacts::validate_name(name)?;
        let manifest_json = read_json_blob(&*self.inner.cas, manifest).await?;
        let parsed_manifest = oci::parse_manifest(&manifest_json).map_err(invalid_image)?;
        let blobs = parsed_manifest.blobs().map_err(invalid_image)?;
        let missing = self.inner.cas.find_missing(&blobs).await?;
        if !missing.is_empty() {
            return Err(ArtifactError::MissingBlobs(missing));
        }

        let record = Record {
            kind: RecordKind::ContainerImage,
            hash: manifest.hash().to_string(),
            size_bytes: manifest.size_bytes(),
            imported_at_unix_millis: unix_millis(SystemTime::now()),
        };

        // The image is only unpacked if it isn't already stored. If it is,
        // but is deleted before the record is written, unpack it after all.
        let image_dir = self.inner.images.join(manifest.hash());
        let mut staged = None;
        if !path_exists(image_dir).await? {
            staged = Some(self.stage_image(&parsed_manifest).await?);
        }
        loop {
            let inner = self.inner.clone();
            let name = name.to_string();
            let record = record.clone();
            let committed = run_blocking(move || commit_import(&inner, name, &record, staged)).await?;
            if let Some(info) = committed {
                return Ok(info);
            }
            staged = Some(self.stage_image(&parsed_manifest).await?);
        }
    }

    async fn list(&self) -> Result<Vec<ArtifactInfo>, ArtifactError> {
        let inner = self.inner.clone();
        run_blocking(move || {
            let mut artifacts = vec![];
            for (name, record) in read_records(&inner)? {
                artifacts.push(artifact_info(&inner, name, &record)?);
            }
            Ok(artifacts)
        })
        .await
    }

    async fn delete(&self, name: &str) -> Result<(), ArtifactError> {
        artifacts::validate_name(name)?;
        let inner = self.inner.clone();
        let name = name.to_string();
        run_blocking(move || {
            let leases = inner.lock_leases();
            let record = read_record(&inner, &name)?;
            let path = inner.refs.join(&name);
            std::fs::remove_file(&path).map_err(|error| io_error("failed to remove", &path, error))?;
            let removed = remove_image_if_unused(&inner, &leases, &record.hash)?;
            drop(leases);
            drop(removed);
            Ok(())
        })
        .await
    }

    async fn container_image(&self, name: &str) -> Result<ContainerImage, ArtifactError> {
        artifacts::validate_name(name)?;
        let inner = self.inner.clone();
        let name = name.to_string();
        run_blocking(move || lease_container_image(&inner, &name)).await
    }

    async fn usage(&self) -> Result<Usage, ArtifactError> {
        let inner = self.inner.clone();
        run_blocking(move || {
            let records = read_records(&inner)?;
            let hashes: BTreeSet<String> = records.into_iter().map(|(_, record)| record.hash).collect();

            let mut usage = Usage::default();
            for hash in hashes {
                usage.artifacts += 1;
                usage.bytes += read_image_record(&inner, &hash)?.size_bytes;
            }
            Ok(usage)
        })
        .await
    }
}

impl FsArtifactStore {
    /// Unpack an image into a new directory under `tmp/`.
    async fn stage_image(&self, manifest: &Manifest) -> Result<TempDir, ArtifactError> {
        let config_digest = manifest.config.cas_digest().map_err(invalid_image)?;
        let config_json = read_json_blob(&*self.inner.cas, &config_digest).await?;
        let config = oci::parse_image_config(&config_json).map_err(invalid_image)?;
        check_platform(&config)?;

        let tmp = self.inner.tmp.clone();
        let staging = run_blocking(move || -> Result<TempDir, ArtifactError> {
            let staging = tempfile::Builder::new()
                .prefix("import-")
                .tempdir_in(&tmp)
                .map_err(|error| io_error("failed to create a directory in", &tmp, error))?;
            create_dir_all(&staging.path().join(ROOTFS_DIR))?;
            create_dir_all(&staging.path().join("layers"))?;
            Ok(staging)
        })
        .await?;

        let mut size_bytes = 0;
        for (index, layer) in manifest.layers.iter().enumerate() {
            size_bytes += self.apply_layer(layer, index, staging.path()).await?;
        }

        let record = ImageRecord {
            env: config.env(),
            size_bytes,
        };
        let staging_dir = staging.path().to_path_buf();
        run_blocking(move || {
            write_json(&staging_dir.join(IMAGE_RECORD_FILE), &record)?;
            // So the image is complete on disk before it is moved into place.
            fsutil::sync_filesystem(&staging_dir).map_err(|error| io_error("failed to sync", &staging_dir, error))
        })
        .await?;
        Ok(staging)
    }

    /// Unpack one layer into the image being staged in `staging`, returning
    /// the size of the files it wrote.
    #[tracing::instrument(skip_all, fields(index))]
    async fn apply_layer(&self, layer: &oci::Descriptor, index: usize, staging: &Path) -> Result<u64, ArtifactError> {
        let compression = oci::layer_compression(&layer.media_type).map_err(invalid_image)?;
        let digest = layer.cas_digest().map_err(invalid_image)?;

        // A hard link to the blob where possible, so nothing is copied.
        let layer_path = staging.join("layers").join(index.to_string());
        self.inner.cas.materialize(&digest, &layer_path, false).await?;

        let rootfs = staging.join(ROOTFS_DIR);
        run_blocking(move || {
            let file = File::open(&layer_path).map_err(|error| io_error("failed to open", &layer_path, error))?;
            let reader = BufReader::new(file);
            let size = match compression {
                Compression::None => unpack::apply_layer(reader, &rootfs),
                Compression::Gzip => unpack::apply_layer(flate2::bufread::MultiGzDecoder::new(reader), &rootfs),
            };
            std::fs::remove_file(&layer_path).map_err(|error| io_error("failed to remove", &layer_path, error))?;
            size
        })
        .await
    }
}

impl Inner {
    fn lock_leases(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        // The map is never left inconsistent, so a panic while it was held
        // doesn't make it unusable.
        self.leases.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Keeps an image's files in place while an action uses them.
struct Lease {
    inner: Arc<Inner>,
    hash: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let inner = self.inner.clone();
        let hash = std::mem::take(&mut self.hash);
        let release = move || {
            let mut leases = inner.lock_leases();
            let count = leases.entry(hash.clone()).or_default();
            *count = count.saturating_sub(1);
            if *count == 0 {
                leases.remove(&hash);
            }
            match remove_image_if_unused(&inner, &leases, &hash) {
                Ok(removed) => {
                    drop(leases);
                    drop(removed);
                }
                Err(error) => tracing::warn!("failed to delete container image {hash}: {error}"),
            }
        };

        // Deleting an image can be slow, so do it in the background.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(release)),
            Err(_) => release(),
        }
    }
}

fn lease_container_image(inner: &Arc<Inner>, name: &str) -> Result<ContainerImage, ArtifactError> {
    let mut leases = inner.lock_leases();
    let record = read_record(inner, name)?;
    let kind = artifact_kind(record.kind);
    if kind != ArtifactKind::ContainerImage {
        return Err(ArtifactError::WrongKind {
            name: name.to_string(),
            expected: ArtifactKind::ContainerImage,
            actual: kind,
        });
    }

    let image = read_image_record(inner, &record.hash)?;
    let digest = record_digest(&record)?;
    *leases.entry(record.hash.clone()).or_default() += 1;
    drop(leases);

    let lease = Lease {
        inner: inner.clone(),
        hash: record.hash.clone(),
    };
    Ok(ContainerImage {
        digest,
        rootfs: inner.images.join(&record.hash).join(ROOTFS_DIR),
        env: image.env,
        lease: Box::new(lease),
    })
}

/// Record the artifact `name`, moving its staged image into place as
/// `images/<hash>` unless that already exists. Returns `None`, changing
/// nothing, if the image needs staging after all.
fn commit_import(
    inner: &Inner,
    name: String,
    record: &Record,
    staged: Option<TempDir>,
) -> Result<Option<ArtifactInfo>, ArtifactError> {
    let leases = inner.lock_leases();
    let image_dir = inner.images.join(&record.hash);
    let image_exists = image_dir.exists();
    // A staged image that turns out not to be needed. Dropping it deletes
    // it, after unlocking.
    let mut discarded = None;
    match staged {
        Some(staged) if image_exists => discarded = Some(staged),
        Some(staged) => install_image(staged, &image_dir)?,
        None if image_exists => {}
        None => return Ok(None),
    }

    let replaced = write_record(inner, &name, record)?;
    let mut removed = None;
    if let Some(replaced) = replaced {
        removed = remove_image_if_unused(inner, &leases, &replaced.hash)?;
    }
    let info = artifact_info(inner, name, record)?;
    drop(leases);
    drop(discarded);
    drop(removed);
    Ok(Some(info))
}

fn install_image(staged: TempDir, image_dir: &Path) -> Result<(), ArtifactError> {
    let layers = staged.path().join("layers");
    std::fs::remove_dir_all(&layers).map_err(|error| io_error("failed to remove", &layers, error))?;
    let staged = staged.keep();
    std::fs::rename(&staged, image_dir).map_err(|error| io_error("failed to rename into", image_dir, error))
}

/// Move an image aside to be deleted if no artifact refers to it and nothing
/// is using it. `leases` must be the locked lease map. Dropping the returned
/// directory deletes the image; that can be slow, so do it after unlocking.
fn remove_image_if_unused(
    inner: &Inner,
    leases: &HashMap<String, usize>,
    hash: &str,
) -> Result<Option<TempDir>, ArtifactError> {
    if leases.contains_key(hash) || referenced_hashes(inner)?.contains(hash) {
        return Ok(None);
    }

    let image_dir = inner.images.join(hash);
    let trash = tempfile::Builder::new()
        .prefix("deleted-")
        .tempdir_in(&inner.tmp)
        .map_err(|error| io_error("failed to create a directory in", &inner.tmp, error))?;
    let target = trash.path().join(hash);
    match std::fs::rename(&image_dir, &target) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("failed to move", &image_dir, error)),
    }

    tracing::info!("deleting unused container image {hash}");
    Ok(Some(trash))
}

/// Write `record` as the artifact `name`, returning the record it replaced.
/// The lease map must be locked.
fn write_record(inner: &Inner, name: &str, record: &Record) -> Result<Option<Record>, ArtifactError> {
    let replaced = match read_record(inner, name) {
        Ok(replaced) => Some(replaced),
        Err(ArtifactError::NotFound(_)) => None,
        Err(error) => return Err(error),
    };

    let mut temp = tempfile::NamedTempFile::new_in(&inner.tmp)
        .map_err(|error| io_error("failed to create a file in", &inner.tmp, error))?;
    let json = serde_json::to_vec(record).map_err(|error| ArtifactError::io("failed to encode record", error.into()))?;
    temp.write_all(&json)
        .and_then(|()| temp.as_file().sync_data())
        .map_err(|error| io_error("failed to write", temp.path(), error))?;

    let path = inner.refs.join(name);
    temp.persist(&path)
        .map_err(|error| io_error("failed to rename into", &path, error.error))?;
    Ok(replaced)
}

fn read_record(inner: &Inner, name: &str) -> Result<Record, ArtifactError> {
    let path = inner.refs.join(name);
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(ArtifactError::NotFound(name.to_string())),
        Err(error) => return Err(io_error("failed to read", &path, error)),
    };
    serde_json::from_slice(&data).map_err(|error| io_error("failed to parse", &path, error.into()))
}

/// Every artifact's record, by name.
fn read_records(inner: &Inner) -> Result<Vec<(String, Record)>, ArtifactError> {
    let entries = std::fs::read_dir(&inner.refs).map_err(|error| io_error("failed to list", &inner.refs, error))?;

    let mut records = vec![];
    for entry in entries {
        let entry = entry.map_err(|error| io_error("failed to list", &inner.refs, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let record = read_record(inner, &name)?;
        records.push((name, record));
    }
    records.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(records)
}

fn referenced_hashes(inner: &Inner) -> Result<BTreeSet<String>, ArtifactError> {
    let records = read_records(inner)?;
    Ok(records.into_iter().map(|(_, record)| record.hash).collect())
}

fn read_image_record(inner: &Inner, hash: &str) -> Result<ImageRecord, ArtifactError> {
    let path = inner.images.join(hash).join(IMAGE_RECORD_FILE);
    let data = std::fs::read(&path).map_err(|error| io_error("failed to read", &path, error))?;
    serde_json::from_slice(&data).map_err(|error| io_error("failed to parse", &path, error.into()))
}

fn artifact_info(inner: &Inner, name: String, record: &Record) -> Result<ArtifactInfo, ArtifactError> {
    let image = read_image_record(inner, &record.hash)?;
    Ok(ArtifactInfo {
        name,
        kind: artifact_kind(record.kind),
        digest: record_digest(record)?,
        size_bytes: image.size_bytes,
        imported_at: UNIX_EPOCH + Duration::from_millis(record.imported_at_unix_millis),
    })
}

fn artifact_kind(kind: RecordKind) -> ArtifactKind {
    match kind {
        RecordKind::ContainerImage => ArtifactKind::ContainerImage,
    }
}

fn record_digest(record: &Record) -> Result<Digest, ArtifactError> {
    Digest::new(&record.hash, record.size_bytes)
        .map_err(|error| ArtifactError::io("invalid artifact record", std::io::Error::other(error)))
}

/// Read a small JSON blob, such as a manifest, from the CAS.
async fn read_json_blob(cas: &dyn Cas, digest: &Digest) -> Result<Vec<u8>, ArtifactError> {
    if digest.size_bytes() > oci::MAX_JSON_SIZE_BYTES {
        let message = format!("{digest} is too large to be a manifest or config");
        return Err(ArtifactError::InvalidImage(message));
    }

    Ok(cas.read_bytes(digest).await?)
}

/// Images must be for Linux on this machine's architecture, or nothing in
/// them will run.
fn check_platform(config: &ImageConfig) -> Result<(), ArtifactError> {
    let host = oci::host_architecture();
    if config.os != "linux" || config.architecture != host {
        let message = format!("the image is for {}/{}, but this machine is linux/{host}", config.os, config.architecture);
        return Err(ArtifactError::InvalidImage(message));
    }
    Ok(())
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), ArtifactError> {
    let json = serde_json::to_vec(value).map_err(|error| ArtifactError::io("failed to encode", error.into()))?;
    std::fs::write(path, json).map_err(|error| io_error("failed to write", path, error))
}

async fn path_exists(path: PathBuf) -> Result<bool, ArtifactError> {
    run_blocking(move || match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("failed to stat", &path, error)),
    })
    .await
}

fn unix_millis(time: SystemTime) -> u64 {
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    since_epoch.as_millis().try_into().unwrap_or(u64::MAX)
}

fn invalid_image(error: OciError) -> ArtifactError {
    ArtifactError::InvalidImage(error.to_string())
}

fn create_dir_all(path: &Path) -> Result<(), ArtifactError> {
    std::fs::create_dir_all(path).map_err(|error| io_error("failed to create", path, error))
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ArtifactError {
    ArtifactError::io(format!("{action} {}", path.display()), error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;

    fn read_file(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    /// Builds an image in a CAS, from layers of (path, contents) files.
    async fn put_image(cas: &MemCas, layers: &[&[(&str, &[u8])]], env: &[&str]) -> Digest {
        let mut layer_descriptors = vec![];
        for layer in layers {
            let data = tar_layer(layer);
            let digest = cas.put_bytes(&data).await.unwrap();
            layer_descriptors.push(descriptor("application/vnd.oci.image.layer.v1.tar", &digest));
        }

        let config = serde_json::json!({
            "architecture": oci::host_architecture(),
            "os": "linux",
            "config": {"Env": env},
        });
        let config = cas.put_bytes(config.to_string().as_bytes()).await.unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": descriptor("application/vnd.oci.image.config.v1+json", &config),
            "layers": layer_descriptors,
        });
        cas.put_bytes(manifest.to_string().as_bytes()).await.unwrap()
    }

    fn descriptor(media_type: &str, digest: &Digest) -> serde_json::Value {
        serde_json::json!({
            "mediaType": media_type,
            "digest": format!("sha256:{}", digest.hash()),
            "size": digest.size_bytes(),
        })
    }

    fn tar_layer(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(vec![]);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            // Set the raw name, so tests can write paths tar::Builder rejects.
            header.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    async fn open(dir: &Path) -> (FsArtifactStore, Arc<MemCas>) {
        let cas = Arc::new(MemCas::new());
        let store = FsArtifactStore::open(dir, cas.clone()).await.unwrap();
        (store, cas)
    }

    /// Wait for background deletion of `path`.
    async fn wait_until_gone(path: &Path) {
        for _ in 0..200 {
            if !path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{} was never deleted", path.display());
    }

    #[tokio::test]
    async fn imports_and_opens_container_images() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let layers: [&[(&str, &[u8])]; 2] = [&[("etc/a", b"lower"), ("etc/b", b"lower")], &[("etc/b", b"upper")]];
        let manifest = put_image(&cas, &layers, &["PATH=/bin"]).await;

        let info = store.import_container_image("toolchain:1", &manifest).await.unwrap();
        let image = store.container_image("toolchain:1").await.unwrap();

        assert_eq!(info.name, "toolchain:1");
        assert_eq!(info.kind, ArtifactKind::ContainerImage);
        assert_eq!(info.digest, manifest);
        assert_eq!(info.size_bytes, 15);
        assert_eq!(image.digest, manifest);
        assert_eq!(image.env, [("PATH".to_string(), "/bin".to_string())]);
        assert_eq!(read_file(&image.rootfs.join("etc/a")), b"lower");
        assert_eq!(read_file(&image.rootfs.join("etc/b")), b"upper");
        assert_eq!(store.list().await.unwrap(), [info]);
        assert_eq!(store.usage().await.unwrap(), Usage { artifacts: 1, bytes: 15 });
    }

    #[tokio::test]
    async fn reports_every_missing_blob() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let layer = cas.put_bytes(&tar_layer(&[])).await.unwrap();
        let config = Digest::of_bytes(b"absent config");
        let manifest = serde_json::json!({
            "config": descriptor("application/vnd.oci.image.config.v1+json", &config),
            "layers": [descriptor("application/vnd.oci.image.layer.v1.tar", &layer)],
        });
        let manifest = cas.put_bytes(manifest.to_string().as_bytes()).await.unwrap();

        let err = store.import_container_image("image", &manifest).await.unwrap_err();

        assert!(matches!(&err, ArtifactError::MissingBlobs(missing) if *missing == [config]), "{err}");
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_images_for_other_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let config = serde_json::json!({"architecture": "not-this-one", "os": "linux"});
        let config = cas.put_bytes(config.to_string().as_bytes()).await.unwrap();
        let manifest = serde_json::json!({
            "config": descriptor("application/vnd.oci.image.config.v1+json", &config),
            "layers": [],
        });
        let manifest = cas.put_bytes(manifest.to_string().as_bytes()).await.unwrap();

        let err = store.import_container_image("image", &manifest).await.unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
    }

    #[tokio::test]
    async fn failed_imports_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let layer: [&[(&str, &[u8])]; 1] = [&[("../escape", b"x")]];
        let manifest = put_image(&cas, &layer, &[]).await;

        let err = store.import_container_image("image", &manifest).await.unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidImage(_)), "{err}");
        assert_eq!(std::fs::read_dir(dir.path().join("tmp")).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(dir.path().join("images")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn deleting_keeps_images_in_use_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let layer: [&[(&str, &[u8])]; 1] = [&[("file", b"data")]];
        let manifest = put_image(&cas, &layer, &[]).await;
        store.import_container_image("image", &manifest).await.unwrap();

        let image = store.container_image("image").await.unwrap();
        store.delete("image").await.unwrap();

        assert!(matches!(store.container_image("image").await, Err(ArtifactError::NotFound(_))));
        assert_eq!(read_file(&image.rootfs.join("file")), b"data");
        let rootfs = image.rootfs.clone();
        drop(image);
        wait_until_gone(&rootfs).await;
    }

    #[tokio::test]
    async fn replacing_an_artifact_deletes_its_old_image() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let old_layer: [&[(&str, &[u8])]; 1] = [&[("version", b"1")]];
        let new_layer: [&[(&str, &[u8])]; 1] = [&[("version", b"2")]];
        let old = put_image(&cas, &old_layer, &[]).await;
        let new = put_image(&cas, &new_layer, &[]).await;

        store.import_container_image("image", &old).await.unwrap();
        let old_rootfs = store.container_image("image").await.unwrap().rootfs;
        store.import_container_image("image", &new).await.unwrap();

        let image = store.container_image("image").await.unwrap();
        assert_eq!(read_file(&image.rootfs.join("version")), b"2");
        wait_until_gone(&old_rootfs).await;
    }

    #[tokio::test]
    async fn artifacts_with_the_same_image_share_it() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = open(dir.path()).await;
        let layer: [&[(&str, &[u8])]; 1] = [&[("file", b"data")]];
        let manifest = put_image(&cas, &layer, &[]).await;
        store.import_container_image("a", &manifest).await.unwrap();
        store.import_container_image("b", &manifest).await.unwrap();

        store.delete("a").await.unwrap();

        let image = store.container_image("b").await.unwrap();
        assert_eq!(read_file(&image.rootfs.join("file")), b"data");
        assert_eq!(store.usage().await.unwrap(), Usage { artifacts: 1, bytes: 4 });
    }

    #[tokio::test]
    async fn artifacts_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let cas = Arc::new(MemCas::new());
        let layer: [&[(&str, &[u8])]; 1] = [&[("file", b"data")]];
        let manifest = put_image(&cas, &layer, &[]).await;
        let store = FsArtifactStore::open(dir.path(), cas.clone()).await.unwrap();
        store.import_container_image("image", &manifest).await.unwrap();
        drop(store);

        let reopened = FsArtifactStore::open(dir.path(), cas).await.unwrap();

        let image = reopened.container_image("image").await.unwrap();
        assert_eq!(read_file(&image.rootfs.join("file")), b"data");
    }

    #[tokio::test]
    async fn rejects_invalid_names() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _cas) = open(dir.path()).await;

        let err = store.delete("../refs").await.unwrap_err();

        assert!(matches!(err, ArtifactError::InvalidName(_)), "{err}");
    }

    #[tokio::test]
    async fn open_fails_while_another_store_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let _first = open(dir.path()).await;

        let result = FsArtifactStore::open(dir.path(), Arc::new(MemCas::new())).await;

        assert!(result.is_err());
    }
}
