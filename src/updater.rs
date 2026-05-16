use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use reqwest::blocking::Client;
use semver::Version;

use crate::download::download_asset_to_temp;
use crate::extract::{extract_tar_gz, extract_zip, sanitise_path};
use crate::github::{GitHubRelease, fetch_latest_release, fetch_release_by_tag, fetch_releases};
use crate::hash::hash_file;
use crate::patch::apply_patch_bundle;
use crate::platform::{
    FULL_ARCHIVE_NAME, platform_patch_asset_name, set_linux_game_executable_permission,
};
use crate::version::{
    InstalledVersion, read_version, verify_installed_files, write_version_atomic,
};

#[derive(Debug)]
pub enum UpdateStatus {
    FirstInstall {
        latest: GitHubRelease,
    },
    CorruptInstall {
        latest: GitHubRelease,
    },
    UpdateAvailable {
        latest: GitHubRelease,
        installed: InstalledVersion,
    },
    UpToDate {
        latest: GitHubRelease,
        installed: InstalledVersion,
    },
}

pub fn check_for_update(
    client: &Client,
    install_dir: impl AsRef<Path>,
) -> anyhow::Result<UpdateStatus> {
    let install_dir = install_dir.as_ref();
    let latest = fetch_latest_release(client)?;
    let Some(installed) = read_version(install_dir)? else {
        return Ok(UpdateStatus::FirstInstall { latest });
    };

    if let Err(err) = verify_installed_files(install_dir, &installed) {
        eprintln!("Installed CraftMoon files failed verification: {err}");
        return Ok(UpdateStatus::CorruptInstall { latest });
    }

    if installed.tag == latest.tag_name {
        Ok(UpdateStatus::UpToDate { latest, installed })
    } else {
        Ok(UpdateStatus::UpdateAvailable { latest, installed })
    }
}

pub fn perform_update(
    client: &Client,
    install_dir: impl AsRef<Path>,
    status: UpdateStatus,
    mut set_status: impl FnMut(String),
    mut set_progress: impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let install_dir = install_dir.as_ref();

    match status {
        UpdateStatus::UpToDate { .. } => Ok(()),
        UpdateStatus::FirstInstall { latest } => {
            set_status(format!("Installing CraftMoon {}...", latest.tag_name));
            install_full_archive(client, install_dir, &latest, set_progress)
        }
        UpdateStatus::CorruptInstall { latest } => {
            set_status("CraftMoon install is corrupted; reinstalling...".to_string());
            install_full_archive(client, install_dir, &latest, set_progress)
        }
        UpdateStatus::UpdateAvailable { latest, installed } => {
            set_status(format!(
                "Updating CraftMoon {} -> {}...",
                installed.tag, latest.tag_name
            ));

            match apply_patch_chain(
                client,
                install_dir,
                &installed,
                &latest,
                &mut set_status,
                &mut set_progress,
            ) {
                Ok(()) => Ok(()),
                Err(err) => {
                    eprintln!(
                        "CraftMoon patch update failed ({err}); falling back to full download."
                    );
                    set_status("Patch failed; downloading full CraftMoon archive...".to_string());
                    install_full_archive(client, install_dir, &latest, set_progress)
                }
            }
        }
    }
}

fn install_full_archive(
    client: &Client,
    install_dir: &Path,
    latest: &GitHubRelease,
    mut set_progress: impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let archive_name = FULL_ARCHIVE_NAME;
    let asset = latest
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "release {} is missing required asset {archive_name}",
                latest.tag_name
            )
        })?;

    let temp = download_asset_to_temp(
        client,
        &asset.browser_download_url,
        &asset.name,
        asset.size,
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

    let version = InstalledVersion::new(latest.tag_name.clone(), files);
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

fn apply_patch_chain(
    client: &Client,
    install_dir: &Path,
    installed: &InstalledVersion,
    latest: &GitHubRelease,
    set_status: &mut impl FnMut(String),
    set_progress: &mut impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let chain = build_patch_chain(client, &installed.tag, &latest.tag_name)?;
    anyhow::ensure!(
        !chain.is_empty(),
        "could not build patch chain from {} to {}",
        installed.tag,
        latest.tag_name
    );

    let mut current_version = installed.clone();
    let total_steps = chain.len();

    for (index, target_tag) in chain.iter().enumerate() {
        let step = index + 1;
        let expected_asset_name = platform_patch_asset_name(&current_version.tag, target_tag);
        set_status(format!(
            "Downloading CraftMoon patch {step}/{total_steps}: {} -> {}...",
            current_version.tag, target_tag
        ));

        let release = fetch_release_by_tag(client, target_tag)?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == expected_asset_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "release {} is missing required patch asset {}",
                    release.tag_name,
                    expected_asset_name
                )
            })?;

        let temp = download_asset_to_temp(
            client,
            &asset.browser_download_url,
            &asset.name,
            asset.size,
            install_dir,
            &mut *set_progress,
        )?;

        set_status(format!(
            "Applying CraftMoon patch {step}/{total_steps}: {} -> {}...",
            current_version.tag, target_tag
        ));
        let patched_version = apply_patch_bundle(temp.path(), install_dir, &current_version)?;
        anyhow::ensure!(
            patched_version.tag == *target_tag,
            "patch bundle target tag mismatch: expected {}, got {}",
            target_tag,
            patched_version.tag
        );
        set_linux_game_executable_permission(install_dir)?;
        write_version_atomic(install_dir, &patched_version)?;
        current_version = patched_version;
    }

    Ok(())
}

fn build_patch_chain(client: &Client, from_tag: &str, to_tag: &str) -> anyhow::Result<Vec<String>> {
    let mut releases = fetch_releases(client)?;
    anyhow::ensure!(
        !releases.is_empty(),
        "GitHub returned no CraftMoon releases"
    );

    releases.sort_by(|a, b| compare_tags(&a.tag_name, &b.tag_name));

    let from_index = releases
        .iter()
        .position(|release| release.tag_name == from_tag)
        .ok_or_else(|| {
            anyhow::anyhow!("installed tag {from_tag} was not found in GitHub releases")
        })?;
    let to_index = releases
        .iter()
        .position(|release| release.tag_name == to_tag)
        .ok_or_else(|| anyhow::anyhow!("latest tag {to_tag} was not found in GitHub releases"))?;

    anyhow::ensure!(
        from_index < to_index,
        "installed tag {from_tag} is not older than latest tag {to_tag}"
    );

    Ok(releases[from_index + 1..=to_index]
        .iter()
        .map(|release| release.tag_name.clone())
        .collect())
}

fn compare_tags(a: &str, b: &str) -> std::cmp::Ordering {
    match (parse_semver_tag(a), parse_semver_tag(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

fn parse_semver_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag)).ok()
}
