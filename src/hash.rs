use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::Context;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub const HASH_BUFFER_SIZE: usize = 64 * 1024;

pub fn hash_file(path: impl AsRef<Path>) -> anyhow::Result<String> {
    let path = path.as_ref();
    let mut hasher = Sha256::new();
    hash_file_into(path, &mut hasher)?;
    Ok(hex_digest(hasher.finalize()))
}

pub fn hash_bytes(bytes: impl AsRef<[u8]>) -> String {
    hex_digest(Sha256::digest(bytes.as_ref()))
}

pub fn hash_directory(path: impl AsRef<Path>) -> anyhow::Result<String> {
    let path = path.as_ref();
    let mut files = Vec::new();

    for entry in WalkDir::new(path) {
        let entry = entry.with_context(|| format!("failed to walk {}", path.display()))?;
        if entry.file_type().is_dir() {
            continue;
        }
        anyhow::ensure!(
            entry.file_type().is_file(),
            "unsupported entry in game install: {}",
            entry.path().display()
        );

        let relative = entry
            .path()
            .strip_prefix(path)
            .with_context(|| format!("failed to make {} relative", entry.path().display()))?
            .to_path_buf();
        let relative = relative_path(&relative)?;
        if relative == "version.json" {
            continue;
        }
        files.push((relative, entry.path().to_path_buf()));
    }

    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    hash_files(files)
}

fn hash_files(files: Vec<(String, std::path::PathBuf)>) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"CraftMoon content hash v1\0");
    for (relative, file_path) in files {
        let metadata = std::fs::metadata(&file_path)
            .with_context(|| format!("failed to inspect {}", file_path.display()))?;
        let path_bytes = relative.as_bytes();
        hasher.update((path_bytes.len() as u64).to_be_bytes());
        hasher.update(path_bytes);
        hasher.update(metadata.len().to_be_bytes());
        hash_file_into(&file_path, &mut hasher)?;
    }

    Ok(hex_digest(hasher.finalize()))
}

fn hash_file_into(path: &Path, hasher: &mut Sha256) -> anyhow::Result<()> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut buffer = [0u8; HASH_BUFFER_SIZE];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {} for hashing", path.display()))?;
        if read == 0 {
            return Ok(());
        }
        hasher.update(&buffer[..read]);
    }
}

fn relative_path(path: &Path) -> anyhow::Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-Unicode game path {}", path.display()))?;
    Ok(path.replace(std::path::MAIN_SEPARATOR, "/"))
}

pub fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn directory_hash_ignores_metadata_and_detects_content_changes() {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "craftmoon-hash-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("alpha.txt"), b"alpha").unwrap();
        std::fs::write(dir.join("nested/beta.bin"), [0, 1, 2, 3]).unwrap();

        let expected = hash_directory(&dir).unwrap();
        std::fs::write(dir.join("version.json"), b"metadata").unwrap();
        assert_eq!(expected, hash_directory(&dir).unwrap());
        std::fs::write(dir.join("nested/beta.bin"), [4, 5, 6]).unwrap();
        assert_ne!(expected, hash_directory(&dir).unwrap());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
