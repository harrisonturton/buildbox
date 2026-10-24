//! Reading container images from OCI image layouts: a directory with an
//! `index.json` and the blobs it refers to, or a tar archive of one.
//!
//! See <https://github.com/opencontainers/image-spec/blob/main/image-layout.md>.

use cas::Digest;
use oci::{Descriptor, Manifest};
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The two bytes every gzip file starts with.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// An OCI image layout on disk.
pub struct ImageLayout {
    dir: PathBuf,

    /// Where an archive was extracted to, deleted when dropped.
    _extracted: Option<TempDir>,
}

/// An image chosen from a layout.
pub struct Image {
    /// The digest of the image's manifest.
    pub manifest_digest: Digest,
    pub manifest: Manifest,
}

impl ImageLayout {
    /// Open the layout at `path`, a directory or a tar archive, which may be
    /// gzipped. An archive is extracted to a temporary directory.
    pub fn open(path: &Path) -> Result<ImageLayout, Box<dyn Error>> {
        if path.is_dir() {
            return Ok(ImageLayout {
                dir: path.to_path_buf(),
                _extracted: None,
            });
        }

        let extracted = extract_archive(path)?;
        Ok(ImageLayout {
            dir: extracted.path().to_path_buf(),
            _extracted: Some(extracted),
        })
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.dir.join("blobs").join("sha256").join(digest.hash())
    }

    /// Choose the image for `platform`, e.g. "linux/amd64". An image whose
    /// platform isn't recorded is chosen if it is the only one.
    pub fn image_for(&self, platform: &str) -> Result<Image, Box<dyn Error>> {
        let index_path = self.dir.join("index.json");
        if !index_path.exists() {
            return Err(format!(
                "{} isn't an OCI image layout: it has no index.json. Export images with `docker save` \
                 (Docker 25 or later), `podman save --format oci-archive` or `skopeo copy`",
                self.dir.display()
            )
            .into());
        }

        let index = oci::parse_index(&read_limited(&index_path)?)?;
        let mut manifests = vec![];
        self.collect_manifests(&index.manifests, &mut manifests)?;

        let descriptor = choose_manifest(&manifests, platform)?;
        let manifest_digest = descriptor.cas_digest()?;
        let manifest = oci::parse_manifest(&read_limited(&self.blob_path(&manifest_digest))?)?;
        Ok(Image {
            manifest_digest,
            manifest,
        })
    }

    /// Add the image manifests in `descriptors` to `manifests`, looking
    /// inside nested indexes.
    fn collect_manifests(&self, descriptors: &[Descriptor], manifests: &mut Vec<Descriptor>) -> Result<(), Box<dyn Error>> {
        for descriptor in descriptors {
            if descriptor.is_manifest() {
                manifests.push(descriptor.clone());
                continue;
            }
            if !descriptor.is_index() {
                continue;
            }

            let path = self.blob_path(&descriptor.cas_digest()?);
            let nested = oci::parse_index(&read_limited(&path)?)?;
            self.collect_manifests(&nested.manifests, manifests)?;
        }
        Ok(())
    }
}

/// The one manifest for `platform`.
fn choose_manifest<'a>(manifests: &'a [Descriptor], platform: &str) -> Result<&'a Descriptor, Box<dyn Error>> {
    let [only] = manifests else {
        return choose_manifest_by_platform(manifests, platform);
    };
    match &only.platform {
        Some(only_platform) if !platform_matches(&only_platform.name(), platform) => {
            Err(format!("the image is for {}, not {platform}", only_platform.name()).into())
        }
        _ => Ok(only),
    }
}

fn choose_manifest_by_platform<'a>(manifests: &'a [Descriptor], platform: &str) -> Result<&'a Descriptor, Box<dyn Error>> {
    let mut matching = manifests.iter().filter(|descriptor| {
        let name = descriptor.platform.as_ref().map(|platform| platform.name());
        name.is_some_and(|name| platform_matches(&name, platform))
    });

    let Some(chosen) = matching.next() else {
        let available: Vec<String> = manifests
            .iter()
            .filter_map(|descriptor| descriptor.platform.as_ref().map(|platform| platform.name()))
            .collect();
        let message = format!("no image for {platform}; the layout has images for: {}", available.join(", "));
        return Err(message.into());
    };
    if matching.next().is_some() {
        return Err(format!("the layout has more than one image for {platform}").into());
    }
    Ok(chosen)
}

/// Whether an image for `available` runs on `wanted`. A platform given
/// without a variant accepts any variant.
fn platform_matches(available: &str, wanted: &str) -> bool {
    available == wanted || available.starts_with(&format!("{wanted}/"))
}

/// Extract a tar archive, gzipped or not, to a new temporary directory.
fn extract_archive(path: &Path) -> Result<TempDir, Box<dyn Error>> {
    let open_error = |error: std::io::Error| format!("failed to open {}: {error}", path.display());
    let mut magic = [0; 2];
    let is_gzip = File::open(path).map_err(open_error)?.read_exact(&mut magic).is_ok() && magic == GZIP_MAGIC;
    let file = BufReader::new(File::open(path).map_err(open_error)?);

    let dir = tempfile::Builder::new().prefix("bb-image-").tempdir()?;
    let unpacked = if is_gzip {
        tar::Archive::new(flate2::bufread::MultiGzDecoder::new(file)).unpack(dir.path())
    } else {
        tar::Archive::new(file).unpack(dir.path())
    };
    unpacked.map_err(|error| format!("failed to extract {}: {error}", path.display()))?;
    Ok(dir)
}

/// Read a small JSON file, such as an index or manifest.
fn read_limited(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let read_error = |error: std::io::Error| format!("failed to read {}: {error}", path.display());
    let file = File::open(path).map_err(read_error)?;
    let mut data = vec![];
    file.take(oci::MAX_JSON_SIZE_BYTES + 1).read_to_end(&mut data).map_err(read_error)?;
    if data.len() as u64 > oci::MAX_JSON_SIZE_BYTES {
        return Err(format!("{} is too large to be an index or manifest", path.display()).into());
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Write a blob into the layout at `dir`, returning its descriptor.
    fn put_blob(dir: &Path, media_type: &str, data: &[u8], platform: Option<&str>) -> serde_json::Value {
        let digest = Digest::of_bytes(data);
        let blobs = dir.join("blobs/sha256");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join(digest.hash()), data).unwrap();

        let mut descriptor = json!({
            "mediaType": media_type,
            "digest": format!("sha256:{}", digest.hash()),
            "size": digest.size_bytes(),
        });
        if let Some(platform) = platform {
            let (os, architecture) = platform.split_once('/').unwrap();
            descriptor["platform"] = json!({"os": os, "architecture": architecture});
        }
        descriptor
    }

    fn put_manifest(dir: &Path, marker: &str, platform: Option<&str>) -> serde_json::Value {
        let config = put_blob(dir, "application/vnd.oci.image.config.v1+json", marker.as_bytes(), None);
        let manifest = json!({"config": config, "layers": []});
        let manifest = manifest.to_string();
        put_blob(dir, oci::MANIFEST_MEDIA_TYPES[0], manifest.as_bytes(), platform)
    }

    fn write_index(dir: &Path, manifests: Vec<serde_json::Value>) {
        let index = json!({"schemaVersion": 2, "manifests": manifests});
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
    }

    fn config_digest(image: &Image) -> String {
        image.manifest.config.digest.clone()
    }

    #[test]
    fn chooses_the_only_image() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = put_manifest(dir.path(), "only", None);
        write_index(dir.path(), vec![manifest.clone()]);

        let image = ImageLayout::open(dir.path()).unwrap().image_for("linux/amd64").unwrap();

        assert_eq!(format!("sha256:{}", image.manifest_digest.hash()), manifest["digest"]);
    }

    #[test]
    fn chooses_by_platform_through_nested_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let amd64 = put_manifest(dir.path(), "amd64", Some("linux/amd64"));
        let arm64 = put_manifest(dir.path(), "arm64", Some("linux/arm64"));
        let nested = json!({"manifests": [amd64, arm64]}).to_string();
        let nested = put_blob(dir.path(), oci::INDEX_MEDIA_TYPES[0], nested.as_bytes(), None);
        write_index(dir.path(), vec![nested]);
        let layout = ImageLayout::open(dir.path()).unwrap();

        let image = layout.image_for("linux/arm64").unwrap();
        let missing = layout.image_for("linux/riscv64");

        let expected = format!("sha256:{}", Digest::of_bytes(b"arm64").hash());
        assert_eq!(config_digest(&image), expected);
        let message = missing.err().unwrap().to_string();
        assert!(message.contains("linux/amd64, linux/arm64"), "{message}");
    }

    #[test]
    fn reads_archives() {
        let dir = tempfile::tempdir().unwrap();
        let layout_dir = dir.path().join("layout");
        std::fs::create_dir(&layout_dir).unwrap();
        let manifest = put_manifest(&layout_dir, "archived", Some("linux/amd64"));
        write_index(&layout_dir, vec![manifest]);
        let archive_path = dir.path().join("image.tar");
        let mut archive = tar::Builder::new(File::create(&archive_path).unwrap());
        archive.append_dir_all(".", &layout_dir).unwrap();
        archive.finish().unwrap();

        let image = ImageLayout::open(&archive_path).unwrap().image_for("linux/amd64").unwrap();

        assert_eq!(config_digest(&image), format!("sha256:{}", Digest::of_bytes(b"archived").hash()));
    }

    #[test]
    fn explains_layouts_without_an_index() {
        let dir = tempfile::tempdir().unwrap();

        let err = ImageLayout::open(dir.path()).unwrap().image_for("linux/amd64").err().unwrap();

        assert!(err.to_string().contains("index.json"), "{err}");
    }

    #[test]
    fn platforms_without_a_variant_match_any_variant() {
        assert!(platform_matches("linux/arm64/v8", "linux/arm64"));
        assert!(platform_matches("linux/arm64", "linux/arm64"));
        assert!(!platform_matches("linux/arm64", "linux/arm"));
    }
}
