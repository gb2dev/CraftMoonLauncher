use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use reqwest::blocking::Client;

use crate::hash::HASH_BUFFER_SIZE;

pub struct TempDownload {
    path: PathBuf,
}

impl TempDownload {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        if self.path.exists()
            && let Err(err) = std::fs::remove_file(&self.path)
        {
            eprintln!(
                "Failed to delete temporary download {}: {err}",
                self.path.display()
            );
        }
    }
}

pub fn download_asset_to_temp(
    client: &Client,
    url: &str,
    asset_name: &str,
    asset_size: u64,
    install_dir: impl AsRef<Path>,
    mut progress: impl FnMut(u64, u64),
) -> anyhow::Result<TempDownload> {
    let install_dir = install_dir.as_ref();
    std::fs::create_dir_all(install_dir).with_context(|| {
        format!(
            "failed to create install directory {}",
            install_dir.display()
        )
    })?;

    let safe_name = Path::new(asset_name)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid asset name {asset_name}"))?;
    let (_, destination) = tempfile::Builder::new()
        .prefix(&format!(".{safe_name}."))
        .suffix(".tmp")
        .tempfile_in(install_dir)
        .with_context(|| {
            format!(
                "failed to create a temporary download in {}",
                install_dir.display()
            )
        })?
        .keep()
        .with_context(|| {
            format!(
                "failed to preserve temporary download in {}",
                install_dir.display()
            )
        })?;
    let temp = TempDownload { path: destination };

    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to start download from {url}"))?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("download failed with HTTP status {status} for {url}");
    }

    let total_size = response.content_length().unwrap_or(asset_size);
    let mut downloaded = 0u64;
    let mut file = File::create(temp.path()).with_context(|| {
        format!(
            "failed to create temporary download {}",
            temp.path().display()
        )
    })?;
    let mut buffer = [0u8; HASH_BUFFER_SIZE];

    loop {
        let read = response
            .read(&mut buffer)
            .with_context(|| format!("failed while downloading {url}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read]).with_context(|| {
            format!(
                "failed to write temporary download {}",
                temp.path().display()
            )
        })?;
        downloaded += read as u64;
        progress(downloaded, total_size);
    }

    if total_size > 0 && downloaded != total_size {
        anyhow::bail!(
            "download from {url} ended early: expected {total_size} bytes, got {downloaded} bytes"
        );
    }

    file.sync_all().with_context(|| {
        format!(
            "failed to sync temporary download {}",
            temp.path().display()
        )
    })?;

    Ok(temp)
}
