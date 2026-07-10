use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use flate2::read::GzDecoder;
use tar::Archive;
use zip::ZipArchive;

pub fn sanitise_path(path: impl AsRef<Path>) -> anyhow::Result<Option<PathBuf>> {
    let path = path.as_ref();
    if path.is_absolute() {
        anyhow::bail!("archive entry path is absolute: {}", path.display());
    }

    let mut sanitized = PathBuf::new();
    let mut saw_component = false;
    let mut is_macosx = false;
    let mut is_ds_store = false;

    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let part_text = part.to_string_lossy();
                if part_text == "__MACOSX" {
                    is_macosx = true;
                }
                if part_text == ".DS_Store" {
                    is_ds_store = true;
                }
                sanitized.push(part);
                saw_component = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("archive entry path contains '..': {}", path.display());
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("archive entry path escapes install dir: {}", path.display());
            }
        }
    }

    if !saw_component || is_macosx || is_ds_store {
        return Ok(None);
    }

    Ok(Some(sanitized))
}

pub fn relative_path_string(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

pub fn extract_zip(
    archive_path: impl AsRef<Path>,
    install_dir: impl AsRef<Path>,
) -> anyhow::Result<Vec<String>> {
    let archive_path = archive_path.as_ref();
    let install_dir = install_dir.as_ref();
    let archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open ZIP archive {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(archive_file)
        .with_context(|| format!("failed to read ZIP archive {}", archive_path.display()))?;
    let mut extracted_files = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(relative_path) = sanitise_path(entry.name())? else {
            continue;
        };
        let relative_path_text = relative_path_string(&relative_path);
        if relative_path_text == "version.json" {
            continue;
        }
        let destination = install_dir.join(&relative_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&destination)
                .with_context(|| format!("failed to create directory {}", destination.display()))?;
            continue;
        }

        anyhow::ensure!(
            entry.is_file() && !entry.is_symlink(),
            "refusing to extract non-file ZIP entry {}",
            entry.name()
        );

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        let mut output = File::create(&destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("failed to extract {}", destination.display()))?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode))
                .with_context(|| {
                    format!("failed to set permissions on {}", destination.display())
                })?;
        }

        extracted_files.push(relative_path_text);
    }

    Ok(extracted_files)
}

pub fn extract_tar_gz(
    archive_path: impl AsRef<Path>,
    install_dir: impl AsRef<Path>,
) -> anyhow::Result<Vec<String>> {
    let archive_path = archive_path.as_ref();
    let install_dir = install_dir.as_ref();
    let archive_file = File::open(archive_path)
        .with_context(|| format!("failed to open tar.gz archive {}", archive_path.display()))?;
    let decoder = GzDecoder::new(archive_file);
    let mut archive = Archive::new(decoder);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    let mut extracted_files = Vec::new();

    for entry in archive.entries().context("failed to read tar.gz entries")? {
        let mut entry = entry.context("failed to read tar.gz entry")?;
        let entry_type = entry.header().entry_type();
        let entry_path = entry
            .path()
            .context("failed to read tar.gz entry path")?
            .to_path_buf();
        let Some(relative_path) = sanitise_path(&entry_path)? else {
            continue;
        };
        let relative_path_text = relative_path_string(&relative_path);
        if relative_path_text == "version.json" {
            continue;
        }
        let destination = install_dir.join(&relative_path);

        if entry_type.is_dir() {
            std::fs::create_dir_all(&destination)
                .with_context(|| format!("failed to create directory {}", destination.display()))?;
            continue;
        }

        anyhow::ensure!(
            entry_type.is_file(),
            "refusing to extract non-file tar entry {}",
            entry_path.display()
        );

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        entry
            .unpack(&destination)
            .with_context(|| format!("failed to extract {}", destination.display()))?;
        extracted_files.push(relative_path_text);
    }

    Ok(extracted_files)
}
