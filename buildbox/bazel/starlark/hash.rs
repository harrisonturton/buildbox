//! Starlark's `hash`, as Bazel defines it: the Java `String.hashCode` of a
//! string. Rules name outputs with it, such as rules_rust's
//! `libfoo-<hash>.rlib`, so bb's must match Bazel's.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;

#[starlark_module]
pub(crate) fn hash_function(builder: &mut GlobalsBuilder) {
    /// The hash of a string, as Bazel computes it.
    fn hash(#[starlark(require = pos)] value: &str) -> anyhow::Result<i32> {
        Ok(java_hash(value))
    }
}

/// Java's `String.hashCode`: over the string's UTF-16 code units, each
/// step multiplying by 31, wrapping at 32 bits.
fn java_hash(text: &str) -> i32 {
    text.encode_utf16().fold(0i32, |hash, unit| hash.wrapping_mul(31).wrapping_add(i32::from(unit)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_match_java() {
        assert_eq!(java_hash(""), 0);
        assert_eq!(java_hash("hello"), 99162322);
        let path = java_hash("buildbox/telemetry/telemetry.rs") as i64;
        let label = java_hash("Label(\"//buildbox/telemetry:telemetry\")") as i64;
        assert_eq!((path + label).abs(), 2152639207);
    }
}
