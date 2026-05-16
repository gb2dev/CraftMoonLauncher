use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::hash::hash_file;

pub const VERSION_FILE_NAME: &str = "version.json";
pub const STABLE_CHANNEL: &str = "stable";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub tag: String,
    pub installed_at: String,
    pub channel: String,
    pub files: BTreeMap<String, String>,
}

impl InstalledVersion {
    pub fn new(tag: impl Into<String>, files: BTreeMap<String, String>) -> Self {
        Self {
            tag: tag.into(),
            installed_at: chrono_like_utc_now(),
            channel: STABLE_CHANNEL.to_string(),
            files,
        }
    }
}

pub fn version_file_path(install_dir: impl AsRef<Path>) -> PathBuf {
    install_dir.as_ref().join(VERSION_FILE_NAME)
}

pub fn read_version(install_dir: impl AsRef<Path>) -> anyhow::Result<Option<InstalledVersion>> {
    let path = version_file_path(install_dir);
    if !path.exists() {
        return Ok(None);
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!(
                "Failed to read {}: {err}; treating install as missing",
                path.display()
            );
            return Ok(None);
        }
    };

    match serde_json::from_str(&content) {
        Ok(version) => Ok(Some(version)),
        Err(err) => {
            eprintln!(
                "Failed to parse {}: {err}; treating install as missing",
                path.display()
            );
            Ok(None)
        }
    }
}

pub fn write_version_atomic(
    install_dir: impl AsRef<Path>,
    version: &InstalledVersion,
) -> anyhow::Result<()> {
    let install_dir = install_dir.as_ref();
    std::fs::create_dir_all(install_dir).with_context(|| {
        format!(
            "failed to create install directory {}",
            install_dir.display()
        )
    })?;

    let target = version_file_path(install_dir);
    let temp = install_dir.join(format!(".{VERSION_FILE_NAME}.tmp"));
    let json = serde_json::to_vec_pretty(version).context("failed to serialize version.json")?;

    std::fs::write(&temp, json)
        .with_context(|| format!("failed to write temporary version file {}", temp.display()))?;
    std::fs::rename(&temp, &target).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            target.display(),
            temp.display()
        )
    })?;

    Ok(())
}

pub fn verify_installed_files(
    install_dir: impl AsRef<Path>,
    version: &InstalledVersion,
) -> anyhow::Result<()> {
    let install_dir = install_dir.as_ref();
    for (relative_path, expected_hash) in &version.files {
        let file_path = install_dir.join(relative_path);
        let actual_hash = hash_file(&file_path).with_context(|| {
            format!(
                "failed to verify installed file {} from version.json",
                relative_path
            )
        })?;
        anyhow::ensure!(
            &actual_hash == expected_hash,
            "installed file hash mismatch for {relative_path}: expected {expected_hash}, got {actual_hash}"
        );
    }

    Ok(())
}

fn chrono_like_utc_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
