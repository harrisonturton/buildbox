//! Identifying each command, as Bazel's invocation ID does.

use ring::rand::{SecureRandom, SystemRandom};

/// A new random (version 4) UUID for a command, in the hyphenated form
/// Bazel gives its invocation IDs.
pub fn new_invocation_id() -> String {
    let mut bytes = [0u8; 16];
    if SystemRandom::new().fill(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_nanos()).unwrap_or_default();
        bytes = (nanos ^ (std::process::id() as u128) << 96).to_le_bytes();
    }
    format_uuid(bytes)
}

/// `bytes` as a version 4, variant 1 UUID.
fn format_uuid(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_version_4_uuids() {
        assert_eq!(format_uuid([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let id = new_invocation_id();
        assert_eq!(id.len(), 36);
        assert_ne!(id, new_invocation_id());
    }
}
