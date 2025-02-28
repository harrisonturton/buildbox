use ring::digest::{Context, Digest, SHA256};
use std::io::Write;

pub fn sha256(data: &[u8]) -> HashDigest {
    let mut hasher = Hasher::sha256();
    hasher.write(data);
    hasher.finish()
}

pub struct Hasher {
    ctx: Context,
}

impl Hasher {
    /// Create a new [`Hasher`] that constructs SHA256 hashes.
    pub fn sha256() -> Self {
        Self {
            ctx: Context::new(&SHA256),
        }
    }

    /// Finish the hash operation and consume the hasher.
    pub fn finish(self) -> HashDigest {
        HashDigest::new(self.ctx.finish())
    }
}

impl Write for Hasher {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.ctx.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub struct HashDigest {
    digest: Digest,
}

impl HashDigest {
    /// Construct a new [`HashDigest`].
    fn new(digest: Digest) -> Self {
        Self { digest }
    }

    /// Get the digest as a lowercase base64 string.
    pub fn base64(&self) -> String {
        let data = self.digest.as_ref();
        data_encoding::HEXLOWER.encode(data)
    }
}
