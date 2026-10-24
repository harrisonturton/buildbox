//! The artifact store: named resources that configure how actions run, such
//! as the container images that container environments run actions in.
//!
//! Artifacts are imported from blobs already in the CAS, so they can be
//! uploaded with the same streaming APIs as any other blob.

use async_trait::async_trait;
use cas::{CasError, Digest};
use std::any::Any;
use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// The longest name an artifact may have.
pub const MAX_NAME_LENGTH: usize = 128;

/// Stores artifacts by name.
#[async_trait]
pub trait ArtifactStore: Send + Sync + 'static {
    /// Import the OCI container image whose manifest has digest `manifest`
    /// under `name`, replacing any artifact already called `name`.
    ///
    /// The manifest, the image config and every layer must be in the CAS.
    async fn import_container_image(&self, name: &str, manifest: &Digest) -> Result<ArtifactInfo, ArtifactError>;

    /// Every artifact, ordered by name.
    async fn list(&self) -> Result<Vec<ArtifactInfo>, ArtifactError>;

    /// Delete the artifact called `name`. Actions already using it are
    /// unaffected.
    async fn delete(&self, name: &str) -> Result<(), ArtifactError>;

    /// Open the container image called `name`, to run an action in.
    async fn container_image(&self, name: &str) -> Result<ContainerImage, ArtifactError>;

    /// Report how much data is stored.
    async fn usage(&self) -> Result<Usage, ArtifactError>;
}

/// A summary of a stored artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactInfo {
    pub name: String,
    pub kind: ArtifactKind,

    /// Identifies the artifact's contents. For a container image, this is
    /// the digest of its manifest.
    pub digest: Digest,

    /// How much space the artifact takes up.
    pub size_bytes: u64,

    pub imported_at: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    ContainerImage,
}

impl fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactKind::ContainerImage => f.write_str("container image"),
        }
    }
}

/// A container image, ready to run actions in.
///
/// The image's files stay in place while this is held, even if the artifact
/// is deleted or replaced.
pub struct ContainerImage {
    /// The digest of the image's manifest.
    pub digest: Digest,

    /// The image's root filesystem, unpacked. It must not be modified.
    pub rootfs: PathBuf,

    /// The environment variables the image sets, as name-value pairs.
    pub env: Vec<(String, String)>,

    /// Keeps the image's files in place until dropped.
    pub lease: Box<dyn Any + Send + Sync>,
}

impl fmt::Debug for ContainerImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContainerImage")
            .field("digest", &self.digest)
            .field("rootfs", &self.rootfs)
            .field("env", &self.env)
            .finish_non_exhaustive()
    }
}

/// How much data an [`ArtifactStore`] holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub artifacts: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("no artifact called {0:?}")]
    NotFound(String),

    #[error(
        "invalid artifact name {0:?}: use 1 to {MAX_NAME_LENGTH} letters, digits, '.', '_', '-' or ':', \
         starting with a letter or digit"
    )]
    InvalidName(String),

    #[error("artifact {name:?} is a {actual}, not a {expected}")]
    WrongKind {
        name: String,
        expected: ArtifactKind,
        actual: ArtifactKind,
    },

    /// Blobs the artifact refers to aren't in the CAS. The client should
    /// upload them and try again.
    #[error("missing blobs: {}", format_digests(.0))]
    MissingBlobs(Vec<Digest>),

    /// The artifact's contents are malformed or unsupported.
    #[error("invalid container image: {0}")]
    InvalidImage(String),

    #[error("{context}: {error}")]
    Io {
        context: String,
        error: std::io::Error,
    },

    #[error(transparent)]
    Cas(CasError),
}

impl ArtifactError {
    pub fn io(context: impl Into<String>, error: std::io::Error) -> ArtifactError {
        ArtifactError::Io {
            context: context.into(),
            error,
        }
    }
}

impl From<CasError> for ArtifactError {
    fn from(error: CasError) -> Self {
        match error {
            CasError::NotFound(digest) => ArtifactError::MissingBlobs(vec![digest]),
            error => ArtifactError::Cas(error),
        }
    }
}

/// Check that `name` can name an artifact. Names are used as file names, so
/// they are restricted to characters that are safe there.
pub fn validate_name(name: &str) -> Result<(), ArtifactError> {
    let is_allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':');
    let starts_well = name.starts_with(|c: char| c.is_ascii_alphanumeric());
    let valid = starts_well && name.len() <= MAX_NAME_LENGTH && name.chars().all(is_allowed);
    if !valid {
        return Err(ArtifactError::InvalidName(name.to_string()));
    }
    Ok(())
}

fn format_digests(digests: &[Digest]) -> String {
    let formatted: Vec<String> = digests.iter().map(Digest::to_string).collect();
    formatted.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_image_style_names() {
        for name in ["ubuntu", "ubuntu:24.04", "gcc-13_x86.64", "a"] {
            validate_name(name).unwrap();
        }
    }

    #[test]
    fn rejects_names_that_arent_safe_file_names() {
        let too_long = "a".repeat(MAX_NAME_LENGTH + 1);
        for name in ["", ".", "..", ".hidden", "a/b", "a b", "-flag", too_long.as_str()] {
            assert!(validate_name(name).is_err(), "{name:?} was accepted");
        }
    }
}
