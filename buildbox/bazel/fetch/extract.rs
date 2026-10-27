//! Extracting archives.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

/// The archive formats Bazel can extract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveType {
    Zip,
    Tar,
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
}

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error("can't tell the archive type of '{0}'; set type explicitly")]
    UnknownType(String),

    #[error("unsupported archive type '{0}'")]
    UnsupportedType(String),

    #[error("prefix '{0}' was given, but not found in the archive")]
    PrefixNotFound(String),

    #[error("archive entry '{0}' is outside the extraction directory")]
    UnsafePath(String),

    #[error("failed to extract {}: {message}", archive.display())]
    Archive { archive: PathBuf, message: String },

    #[error("{context}: {error}")]
    Io { context: String, error: std::io::Error },
}

fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> ExtractError {
    let context = context.into();
    move |error| ExtractError::Io { context, error }
}

impl ArchiveType {
    /// The type named by `type = "..."`, or guessed from a file name.
    pub fn parse(name: &str) -> Result<ArchiveType, ExtractError> {
        let name = name.to_ascii_lowercase();
        let name = name.trim_start_matches('.');
        let ends = |suffix: &str| name == suffix || name.ends_with(&format!(".{suffix}"));
        if ends("zip") || ends("jar") || ends("war") || ends("aar") || ends("nupkg") || ends("whl") {
            return Ok(ArchiveType::Zip);
        }
        if ends("tar.gz") || ends("tgz") || ends("gz") {
            return Ok(ArchiveType::TarGz);
        }
        if ends("tar.xz") || ends("txz") || ends("xz") {
            return Ok(ArchiveType::TarXz);
        }
        if ends("tar.bz2") || ends("tbz") || ends("tbz2") || ends("bz2") {
            return Ok(ArchiveType::TarBz2);
        }
        if ends("tar.zst") || ends("tzst") || ends("zst") {
            return Ok(ArchiveType::TarZst);
        }
        if ends("tar") {
            return Ok(ArchiveType::Tar);
        }
        if ends("7z") || ends("deb") || ends("ar") {
            return Err(ExtractError::UnsupportedType(name.to_string()));
        }
        Err(ExtractError::UnknownType(name.to_string()))
    }
}

/// Extract `archive` into `dest`. Entries outside `strip_prefix` are
/// skipped, and the prefix removed from the rest; then `strip_components`
/// leading components are removed from each, skipping those with no more.
/// `renames` renames entries (by their path in the archive) before
/// either. Blocks.
pub fn extract(
    archive: &Path,
    archive_type: ArchiveType,
    dest: &Path,
    strip_prefix: &str,
    strip_components: usize,
    renames: &HashMap<String, String>,
) -> Result<(), ExtractError> {
    let _span = tracing::info_span!("fetch.extract", archive = %archive.display()).entered();
    std::fs::create_dir_all(dest).map_err(io(format!("failed to create {}", dest.display())))?;
    let prefix = strip_prefix.trim_matches('/');
    let mapper = PathMapper {
        dest,
        prefix,
        strip_components,
        renames,
        found_prefix: prefix.is_empty(),
    };
    let found_prefix = match archive_type {
        ArchiveType::Zip => extract_zip(archive, mapper)?,
        _ => extract_tar(archive, archive_type, mapper)?,
    };
    if !found_prefix {
        return Err(ExtractError::PrefixNotFound(strip_prefix.to_string()));
    }
    Ok(())
}

/// Where each archive entry goes.
struct PathMapper<'a> {
    dest: &'a Path,
    prefix: &'a str,
    strip_components: usize,
    renames: &'a HashMap<String, String>,
    found_prefix: bool,
}

impl PathMapper<'_> {
    /// The destination of an entry, or `None` if it's skipped.
    fn map(&mut self, name: &str) -> Result<Option<PathBuf>, ExtractError> {
        let name = self.renames.get(name).map(String::as_str).unwrap_or(name);
        let name = name.trim_start_matches("./").trim_end_matches('/');
        let relative = if self.prefix.is_empty() {
            name
        } else if name == self.prefix {
            self.found_prefix = true;
            return Ok(None);
        } else if let Some(rest) = name.strip_prefix(self.prefix).and_then(|rest| rest.strip_prefix('/')) {
            self.found_prefix = true;
            rest
        } else {
            return Ok(None);
        };
        let relative = match relative.splitn(self.strip_components + 1, '/').nth(self.strip_components) {
            Some(rest) => rest,
            None => return Ok(None),
        };
        if relative.is_empty() {
            return Ok(None);
        }
        let path = Path::new(relative);
        let safe = path.components().all(|component| matches!(component, Component::Normal(_)));
        if !safe {
            return Err(ExtractError::UnsafePath(name.to_string()));
        }
        Ok(Some(self.dest.join(path)))
    }
}

fn extract_tar(archive: &Path, archive_type: ArchiveType, mut mapper: PathMapper<'_>) -> Result<bool, ExtractError> {
    let file = File::open(archive).map_err(io(format!("failed to open {}", archive.display())))?;
    let reader = BufReader::new(file);
    let decoder: Box<dyn Read> = match archive_type {
        ArchiveType::Tar => Box::new(reader),
        ArchiveType::TarGz => Box::new(flate2::read::MultiGzDecoder::new(reader)),
        ArchiveType::TarXz => Box::new(xz2::read::XzDecoder::new_multi_decoder(reader)),
        ArchiveType::TarBz2 => Box::new(bzip2::read::MultiBzDecoder::new(reader)),
        ArchiveType::TarZst => Box::new(zstd::stream::read::Decoder::new(reader).map_err(io("failed to start zstd"))?),
        ArchiveType::Zip => unreachable!("zip archives aren't tar files"),
    };
    let archive_error = |error: std::io::Error| ExtractError::Archive {
        archive: archive.to_path_buf(),
        message: error.to_string(),
    };
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);
    let mut links = Vec::new();
    for entry in tar.entries().map_err(archive_error)? {
        let mut entry = entry.map_err(archive_error)?;
        let name = entry.path().map_err(archive_error)?.to_string_lossy().into_owned();
        let Some(dest) = mapper.map(&name)? else {
            continue;
        };
        let kind = entry.header().entry_type();
        if kind.is_hard_link() {
            if let Some(target) = entry.link_name().map_err(archive_error)? {
                links.push((target.to_string_lossy().into_owned(), dest));
            }
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io(format!("failed to create {}", parent.display())))?;
        }
        if kind.is_dir() {
            std::fs::create_dir_all(&dest).map_err(io(format!("failed to create {}", dest.display())))?;
            continue;
        }
        if dest.symlink_metadata().is_ok() && !dest.is_dir() {
            let _ = std::fs::remove_file(&dest);
        }
        entry.unpack(&dest).map_err(archive_error)?;
    }
    for (target, dest) in links {
        let Some(source) = mapper.map(&target)? else {
            continue;
        };
        let _ = std::fs::remove_file(&dest);
        std::fs::hard_link(&source, &dest)
            .or_else(|_| std::fs::copy(&source, &dest).map(|_| ()))
            .map_err(io(format!("failed to link {}", dest.display())))?;
    }
    Ok(mapper.found_prefix)
}

fn extract_zip(archive: &Path, mut mapper: PathMapper<'_>) -> Result<bool, ExtractError> {
    let file = File::open(archive).map_err(io(format!("failed to open {}", archive.display())))?;
    let zip_error = |error: zip::result::ZipError| ExtractError::Archive {
        archive: archive.to_path_buf(),
        message: error.to_string(),
    };
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(zip_error)?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(zip_error)?;
        let name = entry.name().to_string();
        let Some(dest) = mapper.map(&name)? else {
            continue;
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(io(format!("failed to create {}", dest.display())))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io(format!("failed to create {}", parent.display())))?;
        }
        let mode = entry.unix_mode();
        let is_symlink = mode.is_some_and(|mode| mode & 0o170000 == 0o120000);
        if is_symlink {
            let mut target = String::new();
            entry.read_to_string(&mut target).map_err(io("failed to read symlink"))?;
            let _ = std::fs::remove_file(&dest);
            std::os::unix::fs::symlink(target, &dest).map_err(io(format!("failed to create {}", dest.display())))?;
            continue;
        }
        let mut out = File::create(&dest).map_err(io(format!("failed to create {}", dest.display())))?;
        std::io::copy(&mut entry, &mut out).map_err(io(format!("failed to write {}", dest.display())))?;
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(mode & 0o777 | 0o200);
            std::fs::set_permissions(&dest, permissions).map_err(io(format!("failed to set mode of {}", dest.display())))?;
        }
    }
    Ok(mapper.found_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_gz(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
        let path = dir.join("archive.tar.gz");
        let file = File::create(&path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, contents.as_bytes()).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn extracts_tar_gz_with_a_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_gz(dir.path(), &[("pkg-1.0/BUILD", "b"), ("pkg-1.0/src/a.c", "a"), ("other.txt", "x")]);
        let dest = dir.path().join("out");
        extract(&archive, ArchiveType::parse("archive.tar.gz").unwrap(), &dest, "pkg-1.0", 0, &HashMap::new()).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("src/a.c")).unwrap(), "a");
        assert!(!dest.join("other.txt").exists());
    }

    #[test]
    fn leading_components_are_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_gz(dir.path(), &[("a/b/c.txt", "c"), ("a/top.txt", "t"), ("d.txt", "d")]);
        let dest = dir.path().join("out");
        extract(&archive, ArchiveType::TarGz, &dest, "", 1, &HashMap::new()).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("b/c.txt")).unwrap(), "c");
        assert_eq!(std::fs::read_to_string(dest.join("top.txt")).unwrap(), "t");
        assert!(!dest.join("d.txt").exists());
    }

    #[test]
    fn missing_prefixes_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_gz(dir.path(), &[("a/b", "")]);
        let error = extract(&archive, ArchiveType::TarGz, &dir.path().join("out"), "nope", 0, &HashMap::new()).unwrap_err();
        assert!(matches!(error, ExtractError::PrefixNotFound(_)));
    }

    #[test]
    fn types_are_guessed_from_names() {
        assert_eq!(ArchiveType::parse("x.tar.xz").unwrap(), ArchiveType::TarXz);
        assert_eq!(ArchiveType::parse("zip").unwrap(), ArchiveType::Zip);
        assert_eq!(ArchiveType::parse("https://x/y.tgz").unwrap(), ArchiveType::TarGz);
        assert!(ArchiveType::parse("noext").is_err());
    }
}
