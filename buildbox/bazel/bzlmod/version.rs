//! Module versions, ordered as Bazel orders them.

use std::cmp::Ordering;
use std::fmt;

/// A module version: dot-separated release identifiers, an optional
/// `-prerelease`, and an optional `+build` that is ignored. The empty
/// version, which non-registry overrides use, is higher than any other.
#[derive(Clone, Debug)]
pub struct Version {
    text: String,
    release: Vec<Identifier>,
    prerelease: Vec<Identifier>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Number(u64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid version '{0}'")]
pub struct VersionError(pub String);

impl Version {
    pub fn parse(text: &str) -> Result<Version, VersionError> {
        let without_build = text.split('+').next().unwrap_or("");
        let (release, prerelease) = match without_build.split_once('-') {
            Some((release, prerelease)) => (release, prerelease),
            None => (without_build, ""),
        };
        let identifiers = |part: &str| -> Result<Vec<Identifier>, VersionError> {
            if part.is_empty() {
                return Ok(Vec::new());
            }
            part.split('.')
                .map(|piece| {
                    if piece.is_empty() || !piece.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                        return Err(VersionError(text.to_string()));
                    }
                    Ok(match piece.parse::<u64>() {
                        Ok(number) => Identifier::Number(number),
                        Err(_) => Identifier::Text(piece.to_string()),
                    })
                })
                .collect()
        };
        Ok(Version {
            text: text.to_string(),
            release: identifiers(release)?,
            prerelease: identifiers(prerelease)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

fn compare_identifiers(a: &[Identifier], b: &[Identifier]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ordering = match (x, y) {
            (Identifier::Number(x), Identifier::Number(y)) => x.cmp(y),
            (Identifier::Number(_), Identifier::Text(_)) => Ordering::Less,
            (Identifier::Text(_), Identifier::Number(_)) => Ordering::Greater,
            (Identifier::Text(x), Identifier::Text(y)) => x.cmp(y),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        match (self.is_empty(), other.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        let release = compare_identifiers(&self.release, &other.release);
        if release != Ordering::Equal {
            return release;
        }
        match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_identifiers(&self.prerelease, &other.prerelease),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Version) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_order_as_bazel_orders_them() {
        let ordered = ["1.0-pre", "1.0", "1.0.1", "1.2", "1.10", "1.10.bcr.1", "2.0.0-rc1", "2.0.0", "20230125.1", ""];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.0+build"), v("1.0"));
        assert!(v("1.3.1.bcr.5") > v("1.3.1"));
    }

    #[test]
    fn invalid_versions_are_rejected() {
        assert!(Version::parse("1..0").is_err());
        assert!(Version::parse("1.0 beta").is_err());
    }
}
