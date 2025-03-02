use ring::digest::{Context, SHA256};
use std::io::Write;

pub fn sha256(data: &[u8]) -> Digest {
    let mut hasher = Hasher::sha256();
    hasher.write(data).expect("failed to write to SHA256 hasher");
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
    pub fn finish(self) -> Digest {
        let digest = self.ctx.finish();
        Digest::sha256(digest.as_ref())
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
pub enum Digest {
    Sha256(String),
}

impl Digest {
    pub fn sha256(digest: &[u8]) -> Self {
        let base64 = data_encoding::HEXLOWER.encode(digest);
        Self::Sha256(base64)
    }
}

impl ToString for Digest {
    fn to_string(&self) -> String {
        match self {
            Digest::Sha256(sha256) => sha256.to_owned(),
        }
    }
}