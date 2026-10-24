//! `apple_common.dotted_version`: version numbers such as `10.11` or
//! `16.0.1`, which the Xcode rules compare.

use allocative::Allocative;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use starlark::{starlark_module, starlark_simple_value};
use std::cmp::Ordering;
use std::fmt;

/// A dotted version. Components compare numerically where they're numbers,
/// and missing components count as zero, so `1.2` equals `1.2.0`.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct DottedVersion {
    pub text: String,
}

starlark_simple_value!(DottedVersion);

impl DottedVersion {
    /// Parse a version, which must be dot-separated components each starting
    /// with a digit.
    pub fn parse(text: &str) -> anyhow::Result<DottedVersion> {
        let valid = text.split('.').all(|component| component.starts_with(|c: char| c.is_ascii_digit()));
        if text.is_empty() || !valid {
            anyhow::bail!("Dotted version components must all start with the form \\d+([a-z0-9]*?)?(\\d+)? but got '{text}'");
        }
        Ok(DottedVersion { text: text.to_string() })
    }

    fn compare(&self, other: &DottedVersion) -> Ordering {
        let mine: Vec<&str> = self.text.split('.').collect();
        let theirs: Vec<&str> = other.text.split('.').collect();
        for index in 0..mine.len().max(theirs.len()) {
            let ordering = compare_component(mine.get(index).copied().unwrap_or("0"), theirs.get(index).copied().unwrap_or("0"));
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }
}

/// Compare components by their leading numbers, then by what follows.
fn compare_component(a: &str, b: &str) -> Ordering {
    let (a_number, a_rest) = split_number(a);
    let (b_number, b_rest) = split_number(b);
    a_number.cmp(&b_number).then_with(|| a_rest.cmp(b_rest))
}

fn split_number(component: &str) -> (u64, &str) {
    let digits = component.find(|c: char| !c.is_ascii_digit()).unwrap_or(component.len());
    let number = component[..digits].parse().unwrap_or(u64::MAX);
    (number, &component[digits..])
}

impl fmt::Display for DottedVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

#[starlark_value(type = "DottedVersion")]
impl<'v> StarlarkValue<'v> for DottedVersion {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("DottedVersion", dotted_version_methods);
        Some(METHODS.methods())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<DottedVersion>().is_some_and(|other| self.compare(other) == Ordering::Equal))
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<Ordering> {
        match other.downcast_ref::<DottedVersion>() {
            Some(other) => Ok(DottedVersion::compare(self, other)),
            None => Err(starlark::Error::new_other(anyhow::anyhow!("can't compare a DottedVersion with {}", other.get_type()))),
        }
    }
}

#[starlark_module]
fn dotted_version_methods(builder: &mut MethodsBuilder) {
    /// -1, 0 or 1 as the version is less than, equal to or greater than
    /// `other`.
    fn compare_to<'v>(this: Value<'v>, other: Value<'v>) -> anyhow::Result<i32> {
        let this = this.downcast_ref::<DottedVersion>().ok_or_else(|| anyhow::anyhow!("expected a DottedVersion"))?;
        let other = other.downcast_ref::<DottedVersion>().ok_or_else(|| anyhow::anyhow!("compare_to needs a DottedVersion, not {}", other.get_type()))?;
        Ok(match this.compare(other) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }
}

/// `dotted_version(version)`. `None` gives `None`, as the Xcode rules pass
/// versions that may be unset.
pub fn dotted_version<'v>(version: Value<'v>, heap: starlark::values::Heap<'v>) -> anyhow::Result<Value<'v>> {
    if version.is_none() {
        return Ok(Value::new_none());
    }
    if let Some(existing) = version.downcast_ref::<DottedVersion>() {
        return Ok(heap.alloc(existing.clone()));
    }
    let text = version.unpack_str().ok_or_else(|| anyhow::anyhow!("dotted_version needs a string, not {}", version.get_type()))?;
    Ok(heap.alloc(DottedVersion::parse(text)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> DottedVersion {
        DottedVersion::parse(text).unwrap()
    }

    #[test]
    fn compares_numerically() {
        assert_eq!(version("10.11").compare(&version("9.0")), Ordering::Greater);
        assert_eq!(version("1.2").compare(&version("1.2.0")), Ordering::Equal);
        assert_eq!(version("16.0.1").compare(&version("16.1")), Ordering::Less);
        assert!(DottedVersion::parse("abc").is_err());
    }
}
