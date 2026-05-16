use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use sha2::{Digest, Sha256};

pub const HASH_BUFFER_SIZE: usize = 64 * 1024;

pub fn hash_file(path: impl AsRef<Path>) -> anyhow::Result<String> {
    let path = path.as_ref();
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; HASH_BUFFER_SIZE];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex_digest(hasher.finalize()))
}

pub fn hash_bytes(bytes: impl AsRef<[u8]>) -> String {
    hex_digest(Sha256::digest(bytes.as_ref()))
}

pub fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    hex::encode(digest)
}
