use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use reqwest::blocking::Client;

use crate::download::download_asset_to_temp;
use crate::extract::{extract_tar_gz, extract_zip, sanitise_path};
use crate::hash::hash_file;
use crate::manifest::Manifest;
use crate::patch::apply_patch_bundle;
use crate::platform::{
    FULL_ARCHIVE_NAME, set_linux_game_executable_permission,
};
use crate::version::{
    InstalledVersion, read_version, verify_installed_files, write_version_atomic,
};

#[derive(Debug)]
pub enum UpdateStatus {
    FirstInstall,
    CorruptInstall,
    UpdateAvailable {
        installed: InstalledVersion,
    },
    UpToDate,
}

pub fn check_for_update(
    manifest: &Manifest,
    install_dir: impl AsRef<Path>,
) -> anyhow::Result<UpdateStatus> {
    let install_dir = install_dir.as_ref();
    let Some(installed) = read_version(install_dir)? else {
        return Ok(UpdateStatus::FirstInstall);
    };

    if let Err(err) = verify_installed_files(install_dir, &installed) {
        eprintln!("Installed CraftMoon files failed verification: {err}");
        return Ok(UpdateStatus::CorruptInstall);
    }

    if installed.tag == manifest.game_version {
        Ok(UpdateStatus::UpToDate)
    } else {
        Ok(UpdateStatus::UpdateAvailable { installed })
    }
}

pub fn perform_update(
    client: &Client,
    install_dir: impl AsRef<Path>,
    manifest: &Manifest,
    status: UpdateStatus,
    mut set_status: impl FnMut(String),
    mut set_progress: impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let install_dir = install_dir.as_ref();

    match status {
        UpdateStatus::UpToDate => Ok(()),
        UpdateStatus::FirstInstall => {
            set_status(format!(
                "Installing CraftMoon {}...",
                manifest.game_version
            ));
            install_full_archive(client, install_dir, manifest, set_progress)
        }
        UpdateStatus::CorruptInstall => {
            set_status("CraftMoon install is corrupted; reinstalling...".to_string());
            install_full_archive(client, install_dir, manifest, set_progress)
        }
        UpdateStatus::UpdateAvailable { installed } => {
            set_status(format!(
                "Updating CraftMoon {} -> {}...",
                installed.tag, manifest.game_version
            ));

            match try_patch_update(
                client,
                install_dir,
                manifest,
                &installed,
                &mut set_status,
                &mut set_progress,
            ) {
                Ok(()) => Ok(()),
                Err(err) => {
                    eprintln!(
                        "CraftMoon patch update failed ({err}); falling back to full download."
                    );
                    set_status("Patch failed; downloading full CraftMoon archive...".to_string());
                    install_full_archive(client, install_dir, manifest, set_progress)
                }
            }
        }
    }
}

fn install_full_archive(
    client: &Client,
    install_dir: &Path,
    manifest: &Manifest,
    mut set_progress: impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let archive_name = FULL_ARCHIVE_NAME;
    let expected_hash = manifest
        .game_archives
        .get(archive_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "manifest does not list archive {archive_name} for this platform"
            )
        })?;

    let temp = download_with_fallback(
        client,
        &manifest.endpoints,
        archive_name,
        expected_hash,
        install_dir,
        &mut set_progress,
    )?;

    let previous_version = read_version(install_dir)?;
    let extracted_files = if archive_name.ends_with(".zip") {
        extract_zip(temp.path(), install_dir)?
    } else {
        extract_tar_gz(temp.path(), install_dir)?
    };

    set_linux_game_executable_permission(install_dir)?;

    let mut files = BTreeMap::new();
    for relative_path in extracted_files {
        let hash = hash_file(install_dir.join(&relative_path))
            .with_context(|| format!("failed to hash extracted file {relative_path}"))?;
        files.insert(relative_path, hash);
    }

    if let Some(previous_version) = previous_version {
        remove_stale_managed_files(install_dir, &previous_version, &files)?;
    }

    let version = InstalledVersion::new(manifest.game_version.clone(), files);
    write_version_atomic(install_dir, &version)?;
    Ok(())
}

fn remove_stale_managed_files(
    install_dir: &Path,
    previous_version: &InstalledVersion,
    new_files: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    for relative_path in previous_version.files.keys() {
        if new_files.contains_key(relative_path) {
            continue;
        }

        let safe_relative = sanitise_path(relative_path)?.ok_or_else(|| {
            anyhow::anyhow!("invalid managed path in version.json: {relative_path}")
        })?;
        let path = install_dir.join(safe_relative);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("failed to remove stale managed file {}", path.display())
                });
            }
        }
    }

    Ok(())
}

fn try_patch_update(
    client: &Client,
    install_dir: &Path,
    manifest: &Manifest,
    installed: &InstalledVersion,
    set_status: &mut impl FnMut(String),
    set_progress: &mut impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let chain = build_patch_chain(manifest, &installed.tag, &manifest.game_version)?;
    anyhow::ensure!(
        !chain.is_empty(),
        "could not build patch chain from {} to {}",
        installed.tag,
        manifest.game_version
    );

    let mut current_version = installed.clone();
    let total_steps = chain.len();

    for (index, (target_tag, patch_name, expected_hash)) in chain.iter().enumerate() {
        let step = index + 1;

        set_status(format!(
            "Downloading CraftMoon patch {step}/{total_steps}: {} -> {target_tag}...",
            current_version.tag
        ));

        let temp = download_with_fallback(
            client,
            &manifest.endpoints,
            patch_name,
            expected_hash,
            install_dir,
            &mut *set_progress,
        )?;

        set_status(format!(
            "Applying CraftMoon patch {step}/{total_steps}: {} -> {target_tag}...",
            current_version.tag
        ));
        let patched_version = apply_patch_bundle(temp.path(), install_dir, &current_version)?;
        anyhow::ensure!(
            patched_version.tag == *target_tag,
            "patch bundle target tag mismatch: expected {target_tag}, got {}",
            patched_version.tag
        );
        set_linux_game_executable_permission(install_dir)?;
        write_version_atomic(install_dir, &patched_version)?;
        current_version = patched_version;
    }

    Ok(())
}

fn build_patch_chain(
    manifest: &Manifest,
    from_tag: &str,
    to_tag: &str,
) -> anyhow::Result<Vec<(String, String, String)>> {
    use std::collections::{HashMap, VecDeque};

    let platform_suffix = if cfg!(windows) { ".patch" } else { "-linux.patch" };
    let mut edges: HashMap<String, Vec<(String, String, String)>> = HashMap::new();

    for (filename, hash) in &manifest.patches {
        if cfg!(windows) && filename.ends_with("-linux.patch") {
            continue;
        }
        if cfg!(not(windows)) && !filename.ends_with("-linux.patch") {
            continue;
        }

        let stem = filename.strip_suffix(platform_suffix).unwrap_or_default();
        if let Some((from, to)) = stem.split_once("-to-") {
            edges
                .entry(from.to_string())
                .or_default()
                .push((to.to_string(), filename.clone(), hash.clone()));
        }
    }

    let mut queue = VecDeque::new();
    let mut visited: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    queue.push_back(from_tag.to_string());
    visited.insert(from_tag.to_string(), Vec::new());

    while let Some(current) = queue.pop_front() {
        if current == to_tag {
            return Ok(visited.remove(&current).unwrap());
        }

        if let Some(neighbors) = edges.get(&current) {
            for (next_tag, patch_name, hash) in neighbors {
                if visited.contains_key(next_tag) {
                    continue;
                }
                let mut path = visited[&current].clone();
                path.push((next_tag.clone(), patch_name.clone(), hash.clone()));
                visited.insert(next_tag.clone(), path);
                queue.push_back(next_tag.clone());
            }
        }
    }

    anyhow::bail!(
        "no patch chain found from {from_tag} to {to_tag} in manifest"
    )
}

pub fn download_with_fallback(
    client: &Client,
    endpoints: &[String],
    filename: &str,
    expected_hash: &str,
    install_dir: &Path,
    progress: &mut impl FnMut(u64, u64),
) -> anyhow::Result<crate::download::TempDownload> {
    let mut last_error = None;

    for (i, endpoint) in endpoints.iter().enumerate() {
        let url = format!("{endpoint}/download/{filename}");

        match download_asset_to_temp(client, &url, filename, 0, install_dir, &mut *progress) {
            Ok(temp) => {
                if !expected_hash.is_empty() {
                    let actual = hash_file(temp.path())?;
                    if actual != expected_hash {
                        eprintln!(
                            "Endpoint {i} ({endpoint}): hash mismatch for {filename} \
                             (expected {expected_hash}, got {actual})"
                        );
                        last_error = Some(anyhow::anyhow!(
                            "hash mismatch from endpoint {i} for {filename}"
                        ));
                        continue;
                    }
                }
                return Ok(temp);
            }
            Err(err) => {
                eprintln!("Endpoint {i} ({endpoint}) failed for {filename}: {err}");
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no download endpoints available")))
}
