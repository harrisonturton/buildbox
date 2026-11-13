//! Holding a file's raw bytes in a Starlark string.
//!
//! Bazel's strings hold bytes, so `repository_ctx.read` can return a binary
//! file and `repository_ctx.file` write it back unchanged. Ours hold UTF-8,
//! so, as Python's `surrogateescape` does, each byte that isn't part of
//! valid UTF-8 is stood for by a character of its own: U+10FF00 plus the
//! byte, from a private-use plane that text doesn't use. Valid UTF-8 is
//! itself, so text reads as it always has.

/// The first of the characters standing for bytes.
const ESCAPE_BASE: u32 = 0x10FF00;

/// A file's contents as a string: valid UTF-8 as itself, every other byte
/// as the character standing for it.
pub fn decode(bytes: Vec<u8>) -> String {
    let bytes = match String::from_utf8(bytes) {
        Ok(text) => return text,
        Err(error) => error.into_bytes(),
    };
    let mut text = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        text.extend(chunk.invalid().iter().map(|byte| escape(*byte)));
    }
    text
}

/// The bytes a string stands for: characters standing for bytes as those
/// bytes, everything else as UTF-8.
pub fn encode(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut buffer = [0; 4];
    for character in text.chars() {
        match unescape(character) {
            Some(byte) => bytes.push(byte),
            None => bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes()),
        }
    }
    bytes
}

fn escape(byte: u8) -> char {
    char::from_u32(ESCAPE_BASE + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

fn unescape(character: char) -> Option<u8> {
    let offset = u32::from(character).checked_sub(ESCAPE_BASE)?;
    u8::try_from(offset).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_itself() {
        let text = "plain ASCII, and é and 日本";
        assert_eq!(decode(text.as_bytes().to_vec()), text);
        assert_eq!(encode(text), text.as_bytes());
    }

    #[test]
    fn binary_round_trips() {
        let mut bytes: Vec<u8> = (0..=255).collect();
        bytes.extend_from_slice(b"\x7fELF\x02\x01\x01\0\xc3\xa9\xe6\x97\xa5\xff\xfe");
        bytes.extend_from_slice("é".as_bytes());
        bytes.push(0xc3);
        let text = decode(bytes.clone());
        assert!(text.starts_with('\0'));
        assert!(text.ends_with(escape(0xc3)));
        assert_eq!(encode(&text), bytes);
    }
}
