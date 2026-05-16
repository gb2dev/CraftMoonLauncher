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
use crate::version::{InstalledVersion, VERSION_FILE_NAME};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchIndex {
    pub from: String,
    pub to: String,
    pub files: BTreeMap<String, PatchFileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    old_version: &InstalledVersion,
) -> anyhow::Result<InstalledVersion> {
    let patch_path = patch_path.as_ref();
    let install_dir = install_dir.as_ref();
    let archive_file = File::open(patch_path)
        .with_context(|| format!("failed to open patch bundle {}", patch_path.display()))?;
    let mut archive = ZipArchive::new(archive_file)
        .with_context(|| format!("failed to read patch bundle {}", patch_path.display()))?;

    let index = read_patch_index(&mut archive)?;
    anyhow::ensure!(
        index.from == old_version.tag,
        "patch bundle is for {} -> {}, but installed tag is {}",
        index.from,
        index.to,
        old_version.tag
    );

    validate_patch_index_against_version(&index, old_version)?;

    let mut new_files = BTreeMap::new();
    let mut pending_changes = Vec::new();

    for (relative_path, entry) in &index.files {
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
                new_files.insert(relative_path.clone(), expected_after.to_string());
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
                new_files.insert(relative_path.clone(), expected_hash.to_string());
            }
            PatchOp::Delete => {
                if let Some(expected_before) = entry.hash_before.as_deref() {
                    let actual_before = hash_file(&disk_path)?;
                    anyhow::ensure!(
                        actual_before == expected_before,
                        "delete hash mismatch for {relative_path}: expected {expected_before}, got {actual_before}"
                    );
                }
                pending_changes.push(PendingPatchChange::Delete { path: disk_path });
            }
            PatchOp::Unchanged => {
                let expected_hash = required_field(entry.hash.as_deref(), relative_path, "hash")?;
                let actual_hash = hash_file(&disk_path)?;
                anyhow::ensure!(
                    actual_hash == expected_hash,
                    "unchanged file hash mismatch for {relative_path}: expected {expected_hash}, got {actual_hash}"
                );
                new_files.insert(relative_path.clone(), expected_hash.to_string());
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

    Ok(InstalledVersion::new(index.to, new_files))
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

fn validate_patch_index_against_version(
    index: &PatchIndex,
    old_version: &InstalledVersion,
) -> anyhow::Result<()> {
    for relative_path in old_version.files.keys() {
        anyhow::ensure!(
            index.files.contains_key(relative_path),
            "patch bundle does not mention previously managed file {relative_path}"
        );
    }

    for (relative_path, entry) in &index.files {
        sanitise_path(relative_path)?
            .ok_or_else(|| anyhow::anyhow!("invalid patch path {relative_path}"))?;
        anyhow::ensure!(
            relative_path != VERSION_FILE_NAME,
            "patch bundle must not modify launcher-managed {VERSION_FILE_NAME}"
        );

        match entry.op {
            PatchOp::Update => {
                let expected_before =
                    required_field(entry.hash_before.as_deref(), relative_path, "hash_before")?;
                let old_hash = old_version.files.get(relative_path).ok_or_else(|| {
                    anyhow::anyhow!("patch updates {relative_path}, but it is not in version.json")
                })?;
                anyhow::ensure!(
                    old_hash == expected_before,
                    "patch hash_before for {relative_path} does not match version.json: patch {expected_before}, version.json {old_hash}"
                );
            }
            PatchOp::Create => {
                anyhow::ensure!(
                    !old_version.files.contains_key(relative_path),
                    "patch creates {relative_path}, but version.json already tracks it"
                );
            }
            PatchOp::Delete => {
                let old_hash = old_version.files.get(relative_path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "patch deletes {relative_path}, but version.json does not track it"
                    )
                })?;
                if let Some(expected_before) = entry.hash_before.as_deref() {
                    anyhow::ensure!(
                        old_hash == expected_before,
                        "patch delete hash_before for {relative_path} does not match version.json: patch {expected_before}, version.json {old_hash}"
                    );
                }
            }
            PatchOp::Unchanged => {
                let expected_hash = required_field(entry.hash.as_deref(), relative_path, "hash")?;
                let old_hash = old_version.files.get(relative_path).ok_or_else(|| {
                    anyhow::anyhow!(
                        "patch marks {relative_path} unchanged, but it is not in version.json"
                    )
                })?;
                anyhow::ensure!(
                    old_hash == expected_hash,
                    "patch unchanged hash for {relative_path} does not match version.json: patch {expected_hash}, version.json {old_hash}"
                );
            }
        }
    }

    Ok(())
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
