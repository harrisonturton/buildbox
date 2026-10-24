//! The parts of the OCI image format buildbox reads: image indexes, image
//! manifests and image configs. Docker's equivalent formats are accepted too.
//!
//! See <https://github.com/opencontainers/image-spec>.

use cas::Digest;
use serde::Deserialize;

pub const INDEX_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];

pub const MANIFEST_MEDIA_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Image manifests and configs are small. This bounds what is read into
/// memory before parsing.
pub const MAX_JSON_SIZE_BYTES: u64 = 4 * 1024 * 1024;

const SHA256_PREFIX: &str = "sha256:";

/// A reference to a blob, with what it holds.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub media_type: String,

    /// The blob's digest, as `<algorithm>:<hex>`.
    pub digest: String,

    pub size: u64,

    /// For a manifest in an index, the platform its image runs on.
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub variant: Option<String>,
}

/// A list of manifests, e.g. one per platform.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    #[serde(default)]
    pub media_type: Option<String>,
    pub manifests: Vec<Descriptor>,
}

/// The blobs that make up an image: its config, and its filesystem layers
/// in the order they are applied.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub media_type: Option<String>,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

/// How an image runs: the platform it is for, and the defaults for
/// processes run in it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ImageConfig {
    pub architecture: String,
    pub os: String,
    #[serde(default)]
    pub config: Option<ContainerConfig>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ContainerConfig {
    /// Environment variables, as `NAME=value`.
    #[serde(rename = "Env", default)]
    pub env: Option<Vec<String>>,
}

/// How a layer's tar archive is compressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OciError {
    #[error("invalid {what}: {message}")]
    Parse { what: &'static str, message: String },

    #[error("unsupported digest {0:?}: only sha256 digests are supported")]
    UnsupportedDigest(String),

    #[error("unsupported media type {media_type:?} for {what}")]
    UnsupportedMediaType { what: &'static str, media_type: String },
}

impl Descriptor {
    /// The blob's digest, as stored in the CAS.
    pub fn cas_digest(&self) -> Result<Digest, OciError> {
        let Some(hash) = self.digest.strip_prefix(SHA256_PREFIX) else {
            return Err(OciError::UnsupportedDigest(self.digest.clone()));
        };
        Digest::new(hash, self.size).map_err(|_| OciError::UnsupportedDigest(self.digest.clone()))
    }

    pub fn is_index(&self) -> bool {
        INDEX_MEDIA_TYPES.contains(&self.media_type.as_str())
    }

    pub fn is_manifest(&self) -> bool {
        MANIFEST_MEDIA_TYPES.contains(&self.media_type.as_str())
    }
}

impl Platform {
    /// The platform, as `os/architecture[/variant]`, e.g. `linux/arm64/v8`.
    pub fn name(&self) -> String {
        match &self.variant {
            Some(variant) => format!("{}/{}/{variant}", self.os, self.architecture),
            None => format!("{}/{}", self.os, self.architecture),
        }
    }
}

impl Manifest {
    /// Every blob the image is made of, the config first.
    pub fn blobs(&self) -> Result<Vec<Digest>, OciError> {
        let mut blobs = vec![self.config.cas_digest()?];
        for layer in &self.layers {
            blobs.push(layer.cas_digest()?);
        }
        Ok(blobs)
    }
}

impl ImageConfig {
    /// The environment variables the image sets, as name-value pairs.
    pub fn env(&self) -> Vec<(String, String)> {
        let Some(env) = self.config.as_ref().and_then(|config| config.env.as_ref()) else {
            return vec![];
        };

        let mut variables = vec![];
        for entry in env {
            // An entry without '=' is unset in the image; there is nothing
            // to pass on.
            if let Some((name, value)) = entry.split_once('=') {
                variables.push((name.to_string(), value.to_string()));
            }
        }
        variables
    }
}

pub fn parse_index(data: &[u8]) -> Result<Index, OciError> {
    parse_json("image index", data)
}

/// Parse an image manifest, rejecting other kinds of document.
pub fn parse_manifest(data: &[u8]) -> Result<Manifest, OciError> {
    let manifest: Manifest = parse_json("image manifest", data)?;
    if let Some(media_type) = &manifest.media_type {
        if !MANIFEST_MEDIA_TYPES.contains(&media_type.as_str()) {
            return Err(OciError::UnsupportedMediaType {
                what: "an image manifest",
                media_type: media_type.clone(),
            });
        }
    }
    Ok(manifest)
}

pub fn parse_image_config(data: &[u8]) -> Result<ImageConfig, OciError> {
    parse_json("image config", data)
}

/// How a layer with the given media type is compressed. Fails for media
/// types that aren't filesystem layers, or whose compression isn't
/// supported.
pub fn layer_compression(media_type: &str) -> Result<Compression, OciError> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar" | "application/vnd.oci.image.layer.nondistributable.v1.tar" => {
            Ok(Compression::None)
        }
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        | "application/vnd.docker.image.rootfs.foreign.diff.tar.gzip" => Ok(Compression::Gzip),
        _ => Err(OciError::UnsupportedMediaType {
            what: "a layer",
            media_type: media_type.to_string(),
        }),
    }
}

/// This machine's architecture, as OCI platforms name it.
pub fn host_architecture() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
}

fn parse_json<'a, T: Deserialize<'a>>(what: &'static str, data: &'a [u8]) -> Result<T, OciError> {
    serde_json::from_slice(data).map_err(|error| OciError::Parse {
        what,
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae";

    #[test]
    fn parses_manifests_and_their_blobs() {
        let json = format!(
            r#"{{
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": {{"mediaType": "application/vnd.oci.image.config.v1+json", "digest": "sha256:{HASH}", "size": 3}},
                "layers": [{{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": "sha256:{HASH}", "size": 3}}]
            }}"#
        );

        let manifest = parse_manifest(json.as_bytes()).unwrap();

        let digest = Digest::new(HASH, 3).unwrap();
        assert_eq!(manifest.blobs().unwrap(), [digest.clone(), digest]);
        assert_eq!(layer_compression(&manifest.layers[0].media_type).unwrap(), Compression::Gzip);
    }

    #[test]
    fn rejects_indexes_where_a_manifest_is_expected() {
        let json = r#"{"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [],
                       "config": {"mediaType": "x", "digest": "sha256:00", "size": 0}, "layers": []}"#;

        let err = parse_manifest(json.as_bytes()).unwrap_err();

        assert!(matches!(err, OciError::UnsupportedMediaType { .. }), "{err}");
    }

    #[test]
    fn rejects_other_digest_algorithms() {
        let descriptor = Descriptor {
            media_type: "x".to_string(),
            digest: format!("sha512:{HASH}"),
            size: 3,
            platform: None,
        };

        assert!(matches!(descriptor.cas_digest(), Err(OciError::UnsupportedDigest(_))));
    }

    #[test]
    fn reads_the_environment_from_image_configs() {
        let json = r#"{"architecture": "amd64", "os": "linux",
                       "config": {"Env": ["PATH=/usr/bin:/bin", "EMPTY=", "UNSET", "EQUALS=a=b"]}}"#;

        let config = parse_image_config(json.as_bytes()).unwrap();

        let expected = [("PATH", "/usr/bin:/bin"), ("EMPTY", ""), ("EQUALS", "a=b")];
        let expected: Vec<(String, String)> = expected.map(|(n, v)| (n.to_string(), v.to_string())).to_vec();
        assert_eq!(config.env(), expected);
    }

    #[test]
    fn rejects_unsupported_layer_compression() {
        let err = layer_compression("application/vnd.oci.image.layer.v1.tar+zstd").unwrap_err();

        assert!(matches!(err, OciError::UnsupportedMediaType { .. }), "{err}");
    }

    #[test]
    fn names_platforms() {
        let platform = Platform {
            architecture: "arm64".to_string(),
            os: "linux".to_string(),
            variant: Some("v8".to_string()),
        };

        assert_eq!(platform.name(), "linux/arm64/v8");
    }
}
