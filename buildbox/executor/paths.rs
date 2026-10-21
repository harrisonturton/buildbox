//! Validation of paths that come from clients, so they can't escape the
//! sandbox.

use std::path::{Component, Path, PathBuf};

/// Check that `name` is a single path component, as used for the entries of
/// `Directory` protos.
pub fn validate_name(name: &str) -> Result<(), String> {
    let is_special = name.is_empty() || name == "." || name == "..";
    if is_special || name.contains('/') || name.contains('\0') {
        return Err(format!("invalid file name {name:?}"));
    }
    Ok(())
}

/// Parse a path that must stay within the directory it is relative to.
///
/// The empty path is allowed, and refers to the directory itself.
pub fn parse_relative_path(path: &str) -> Result<PathBuf, String> {
    if path.contains('\0') {
        return Err(format!("invalid path {path:?}"));
    }

    let mut parsed = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parsed.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path {path:?} must be relative and must not contain '..'"));
            }
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_accepts_plain_names() {
        for name in ["file", "file.txt", ".hidden", "..."] {
            assert_eq!(validate_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn validate_name_rejects_special_and_nested_names() {
        for name in ["", ".", "..", "a/b", "/a", "a\0b"] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn parse_relative_path_normalises() {
        assert_eq!(parse_relative_path(""), Ok(PathBuf::new()));
        assert_eq!(parse_relative_path("a/b"), Ok(PathBuf::from("a/b")));
        assert_eq!(parse_relative_path("./a//b/"), Ok(PathBuf::from("a/b")));
    }

    #[test]
    fn parse_relative_path_rejects_escapes() {
        for path in ["/etc/passwd", "..", "a/../../b", "a/..", "a\0b"] {
            assert!(parse_relative_path(path).is_err(), "{path:?}");
        }
    }
}
