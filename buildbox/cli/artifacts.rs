//! `buildbox artifacts`: manage a running server's artifacts, such as the
//! container images that container environments run actions in.

use crate::client::ClientArgs;
use crate::format;
use crate::image_layout::{Image, ImageLayout};
use admin_rust_connect::buildbox::admin::v1::{Artifact, ArtifactKind};
use cas::Digest;
use clap::{Args, Subcommand};
use std::collections::BTreeSet;
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Args, Debug)]
pub struct ArtifactsArgs {
    #[command(subcommand)]
    command: ArtifactsCommand,
}

#[derive(Subcommand, Debug)]
enum ArtifactsCommand {
    /// List the server's artifacts.
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        client: ClientArgs,
    },

    /// Import a container image from an OCI image layout: a directory, or a
    /// tar archive of one, such as `podman save --format oci-archive` or
    /// rules_oci's `oci_image` write. Replaces any artifact with the same
    /// name.
    ImportImage(ImportImageArgs),

    /// Delete an artifact. Actions already using it are unaffected.
    #[command(visible_alias = "rm")]
    Delete {
        /// The artifact to delete.
        name: String,

        #[command(flatten)]
        client: ClientArgs,
    },
}

#[derive(Args, Debug)]
struct ImportImageArgs {
    /// The image layout directory or archive.
    path: PathBuf,

    /// The name to give the image, which environments refer to it by, e.g.
    /// "ubuntu-gcc:13".
    #[arg(long)]
    name: String,

    /// The platform to import, if the layout has images for several. This
    /// must be the server's platform.
    #[arg(long, default_value_t = default_platform())]
    platform: String,

    #[command(flatten)]
    client: ClientArgs,
}

pub async fn run(args: ArtifactsArgs) -> Result<(), Box<dyn Error>> {
    match args.command {
        ArtifactsCommand::List { client } => list(client).await,
        ArtifactsCommand::ImportImage(args) => import_image(args).await,
        ArtifactsCommand::Delete { name, client } => delete(name, client).await,
    }
}

async fn list(client: ClientArgs) -> Result<(), Box<dyn Error>> {
    let admin = client.connect()?;
    let artifacts = admin.list_artifacts().await?;
    if artifacts.is_empty() {
        println!("No artifacts.");
        return Ok(());
    }

    println!("{:<32}{:<18}{:<12}{:<14}DIGEST", "NAME", "KIND", "SIZE", "IMPORTED");
    let now = SystemTime::now();
    for artifact in &artifacts {
        println!("{}", artifact_line(artifact, now));
    }
    Ok(())
}

fn artifact_line(artifact: &Artifact, now: SystemTime) -> String {
    let kind = match artifact.kind.as_known() {
        Some(ArtifactKind::ARTIFACT_KIND_CONTAINER_IMAGE) => "container image",
        Some(ArtifactKind::ARTIFACT_KIND_UNSPECIFIED) | None => "unknown",
    };
    let name = format::truncate(&artifact.name, 31);
    let size = format::bytes(artifact.size_bytes);
    let imported_at = UNIX_EPOCH + Duration::from_millis(artifact.imported_at_unix_millis);
    let age = now.duration_since(imported_at).unwrap_or(Duration::ZERO);
    let imported = format!("{} ago", format::duration(age));
    format!("{name:<32}{kind:<18}{size:<12}{imported:<14}{}", artifact.digest)
}

async fn import_image(args: ImportImageArgs) -> Result<(), Box<dyn Error>> {
    let admin = args.client.connect()?;
    let blobs = args.client.connect_blobs()?;

    // Reading the layout may mean extracting a large archive.
    let path = args.path.clone();
    let platform = args.platform.clone();
    let (layout, image) = tokio::task::spawn_blocking(move || -> Result<(ImageLayout, Image), String> {
        let layout = ImageLayout::open(&path).map_err(|error| error.to_string())?;
        let image = layout.image_for(&platform).map_err(|error| error.to_string())?;
        Ok((layout, image))
    })
    .await??;

    let mut digests = BTreeSet::from([image.manifest_digest.clone()]);
    digests.extend(image.manifest.blobs()?);
    let digests: Vec<Digest> = digests.into_iter().collect();
    let missing = blobs.find_missing(&digests).await?;
    let missing_bytes: u64 = missing.iter().map(Digest::size_bytes).sum();
    println!(
        "Uploading {} of {} blobs ({})",
        missing.len(),
        digests.len(),
        format::bytes(missing_bytes)
    );
    for digest in &missing {
        blobs.upload_file(&layout.blob_path(digest), digest).await?;
    }

    println!("Unpacking {}", args.name);
    let artifact = admin.import_container_image(&args.name, &image.manifest_digest).await?;
    println!(
        "Imported {} ({}, {})",
        artifact.name,
        format::bytes(artifact.size_bytes),
        artifact.digest
    );
    Ok(())
}

async fn delete(name: String, client: ClientArgs) -> Result<(), Box<dyn Error>> {
    let admin = client.connect()?;
    admin.delete_artifact(&name).await?;
    println!("Deleted {name}");
    Ok(())
}

/// Linux on this machine's architecture: the server is usually this
/// machine.
fn default_platform() -> String {
    format!("linux/{}", oci::host_architecture())
}
