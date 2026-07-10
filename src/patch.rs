use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use qbsdiff::Bspatch;
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::extract::sanitise_path;
use crate::hash::{hash_bytes, hash_file};
use crate::version::VERSION_FILE_NAME;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchIndex {
    pub from: String,
    pub to: String,
    pub files: BTreeMap<String, PatchFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchFileEntry {
    pub op: PatchOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bsdiff: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatchOp {
    Update,
    Create,
    Delete,
    Unchanged,
}

pub fn bsdiff_entry_name(relative_path: &str) -> String {
    format!("{}.bsdiff", relative_path.replace('/', "__"))
}

pub fn create_entry_name(relative_path: &str) -> String {
    format!("create/{relative_path}")
}

pub fn apply_patch_bundle(
    patch_path: impl AsRef<Path>,
    install_dir: impl AsRef<Path>,
    installed_tag: &str,
) -> anyhow::Result<String> {
    let patch_path = patch_path.as_ref();
    let install_dir = install_dir.as_ref();
    let archive_file = File::open(patch_path)
        .with_context(|| format!("failed to open patch bundle {}", patch_path.display()))?;
    let mut archive = ZipArchive::new(archive_file)
        .with_context(|| format!("failed to read patch bundle {}", patch_path.display()))?;

    let index = read_patch_index(&mut archive)?;
    anyhow::ensure!(
        index.from == installed_tag,
        "patch bundle is for {} -> {}, but installed tag is {installed_tag}",
        index.from,
        index.to,
    );
    let mut pending_changes = Vec::new();
    for (relative_path, entry) in &index.files {
        anyhow::ensure!(
            relative_path != VERSION_FILE_NAME,
            "patch bundle must not modify launcher-managed {VERSION_FILE_NAME}"
        );
        let disk_path = safe_install_path(install_dir, relative_path)?;

        match entry.op {
            PatchOp::Update => {
                let expected_before =
                    required_field(entry.hash_before.as_deref(), relative_path, "hash_before")?;
                let expected_after =
                    required_field(entry.hash_after.as_deref(), relative_path, "hash_after")?;
                let patch_entry_name =
                    required_field(entry.bsdiff.as_deref(), relative_path, "bsdiff")?;
                sanitise_patch_member_name(patch_entry_name)?;

                let actual_before = hash_file(&disk_path)?;
                anyhow::ensure!(
                    actual_before == expected_before,
                    "pre-patch hash mismatch for {relative_path}: expected {expected_before}, got {actual_before}"
                );

                let old_data = std::fs::read(&disk_path)
                    .with_context(|| format!("failed to read old file {}", disk_path.display()))?;
                let patch_data = read_zip_entry(&mut archive, patch_entry_name)?;
                let mut patched = Vec::new();
                Bspatch::new(&patch_data)
                    .context("failed to parse bsdiff data")?
                    .apply(&old_data, Cursor::new(&mut patched))
                    .with_context(|| format!("failed to apply bsdiff to {relative_path}"))?;
                let actual_after = hash_bytes(&patched);
                anyhow::ensure!(
                    actual_after == expected_after,
                    "post-patch hash mismatch for {relative_path}: expected {expected_after}, got {actual_after}"
                );
                pending_changes.push(PendingPatchChange::Write {
                    path: disk_path,
                    data: patched,
                });
            }
            PatchOp::Create => {
                anyhow::ensure!(
                    !disk_path.exists(),
                    "patch creates {relative_path}, but a file already exists at that path"
                );
                let expected_hash = required_field(entry.hash.as_deref(), relative_path, "hash")?;
                let create_entry = create_entry_name(relative_path);
                sanitise_patch_member_name(&create_entry)?;
                let data = read_zip_entry(&mut archive, &create_entry)?;
                let actual_hash = hash_bytes(&data);
                anyhow::ensure!(
                    actual_hash == expected_hash,
                    "created file hash mismatch for {relative_path}: expected {expected_hash}, got {actual_hash}"
                );
                pending_changes.push(PendingPatchChange::Write {
                    path: disk_path,
                    data,
                });
            }
            PatchOp::Delete => {
                let expected_before =
                    required_field(entry.hash_before.as_deref(), relative_path, "hash_before")?;
                let actual_before = hash_file(&disk_path)?;
                anyhow::ensure!(
                    actual_before == expected_before,
                    "delete hash mismatch for {relative_path}: expected {expected_before}, got {actual_before}"
                );
                pending_changes.push(PendingPatchChange::Delete { path: disk_path });
            }
            PatchOp::Unchanged => {
                let expected_hash = required_field(entry.hash.as_deref(), relative_path, "hash")?;
                let actual_hash = hash_file(&disk_path)?;
                anyhow::ensure!(
                    actual_hash == expected_hash,
                    "unchanged file hash mismatch for {relative_path}: expected {expected_hash}, got {actual_hash}"
                );
            }
        }
    }

    for change in pending_changes {
        match change {
            PendingPatchChange::Write { path, data } => write_file_atomic(&path, &data)?,
            PendingPatchChange::Delete { path } => std::fs::remove_file(&path)
                .with_context(|| format!("failed to delete {}", path.display()))?,
        }
    }

    Ok(index.to)
}

fn read_patch_index(archive: &mut ZipArchive<File>) -> anyhow::Result<PatchIndex> {
    let mut index_file = archive
        .by_name("patch.index")
        .context("patch bundle is missing patch.index")?;
    let mut index_json = String::new();
    index_file
        .read_to_string(&mut index_json)
        .context("failed to read patch.index")?;
    serde_json::from_str(&index_json).context("failed to parse patch.index")
}

fn required_field<'a>(
    field: Option<&'a str>,
    relative_path: &str,
    field_name: &str,
) -> anyhow::Result<&'a str> {
    field.ok_or_else(|| anyhow::anyhow!("patch entry {relative_path} is missing {field_name}"))
}

fn safe_install_path(install_dir: &Path, relative_path: &str) -> anyhow::Result<PathBuf> {
    let relative = sanitise_path(relative_path)?
        .ok_or_else(|| anyhow::anyhow!("invalid install-relative path {relative_path}"))?;
    Ok(install_dir.join(relative))
}

fn sanitise_patch_member_name(path: &str) -> anyhow::Result<()> {
    sanitise_path(path)?.ok_or_else(|| anyhow::anyhow!("invalid patch member path {path}"))?;
    Ok(())
}

fn read_zip_entry(archive: &mut ZipArchive<File>, entry_name: &str) -> anyhow::Result<Vec<u8>> {
    let mut entry = archive
        .by_name(entry_name)
        .with_context(|| format!("patch bundle is missing {entry_name}"))?;
    let mut data = Vec::new();
    entry
        .read_to_end(&mut data)
        .with_context(|| format!("failed to read {entry_name} from patch bundle"))?;
    Ok(data)
}

enum PendingPatchChange {
    Write { path: PathBuf, data: Vec<u8> },
    Delete { path: PathBuf },
}

fn write_file_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file path {}", path.display()))?;
    let temp_path = path.with_file_name(format!(".{file_name}.patch-tmp"));

    let mut file = File::create(&temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    file.write_all(data)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::hash::hash_directory;
    use crate::make_patch::{PatchPlatform, make_patch};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn generated_patch_reaches_the_expected_tree() {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "craftmoon-patch-test-{}-{sequence}",
            std::process::id()
        ));
        let old_dir = root.join("old");
        let new_dir = root.join("new");
        let install_dir = root.join("install");
        let out_dir = root.join("patches");

        std::fs::create_dir_all(old_dir.join("data")).unwrap();
        std::fs::write(old_dir.join("CraftMoon-linux.x86_64"), b"old game").unwrap();
        std::fs::write(old_dir.join("data/removed.bin"), b"remove me").unwrap();
        std::fs::write(old_dir.join("data/unchanged.bin"), b"unchanged").unwrap();

        std::fs::create_dir_all(new_dir.join("data")).unwrap();
        std::fs::write(new_dir.join("CraftMoon-linux.x86_64"), b"new game").unwrap();
        std::fs::write(new_dir.join("data/created.bin"), b"created").unwrap();
        std::fs::write(new_dir.join("data/unchanged.bin"), b"unchanged").unwrap();

        make_patch(
            &old_dir,
            &new_dir,
            "0.4",
            "0.5",
            &out_dir,
            PatchPlatform::Linux,
        )
        .unwrap();
        std::fs::create_dir_all(install_dir.join("data")).unwrap();
        std::fs::copy(
            old_dir.join("CraftMoon-linux.x86_64"),
            install_dir.join("CraftMoon-linux.x86_64"),
        )
        .unwrap();
        std::fs::copy(
            old_dir.join("data/removed.bin"),
            install_dir.join("data/removed.bin"),
        )
        .unwrap();
        std::fs::copy(
            old_dir.join("data/unchanged.bin"),
            install_dir.join("data/unchanged.bin"),
        )
        .unwrap();

        let patch = out_dir.join("0.4-to-0.5-linux.patch");
        assert_eq!(
            apply_patch_bundle(&patch, &install_dir, "0.4").unwrap(),
            "0.5"
        );
        assert_eq!(
            hash_directory(&install_dir).unwrap(),
            hash_directory(&new_dir).unwrap()
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
