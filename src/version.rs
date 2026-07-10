use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const VERSION_FILE_NAME: &str = "version.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledVersion {
    pub tag: String,
}

impl InstalledVersion {
    pub fn new(tag: impl Into<String>) -> Self {
        Self { tag: tag.into() }
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

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let version: InstalledVersion = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    anyhow::ensure!(
        !version.tag.is_empty(),
        "{} has an empty tag",
        path.display()
    );
    Ok(Some(version))
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
