use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Context;
use reqwest::blocking::Client;
use walkdir::WalkDir;

use crate::download::download_asset_to_temp;
use crate::extract::{extract_tar_gz, extract_zip};
use crate::hash::{hash_directory, hash_file};
use crate::manifest::Manifest;
use crate::patch::apply_patch_bundle;
use crate::platform::{CURRENT_PLATFORM, set_linux_game_executable_permission};
use crate::version::{InstalledVersion, read_version, write_version_atomic};

#[derive(Debug)]
pub enum UpdateStatus {
    FirstInstall,
    ReinstallRequired,
    UpdateAvailable { installed: InstalledVersion },
    UpToDate,
}

pub fn recover_install(install_dir: impl AsRef<Path>) -> anyhow::Result<()> {
    let install_dir = install_dir.as_ref();
    let staging_dir = sibling_path(install_dir, "staging")?;
    let backup_dir = sibling_path(install_dir, "previous")?;

    if !install_dir.exists() && backup_dir.exists() {
        std::fs::rename(&backup_dir, install_dir).with_context(|| {
            format!(
                "failed to restore previous CraftMoon install {}",
                install_dir.display()
            )
        })?;
    }

    remove_path(&staging_dir)?;
    if backup_dir.exists() {
        remove_path(&backup_dir)?;
    }
    Ok(())
}

pub fn check_for_update(
    manifest: &Manifest,
    install_dir: impl AsRef<Path>,
) -> anyhow::Result<UpdateStatus> {
    let install_dir = install_dir.as_ref();
    let installed = match read_version(install_dir) {
        Ok(Some(version)) => version,
        Ok(None) => return Ok(UpdateStatus::FirstInstall),
        Err(err) => {
            eprintln!("Installed CraftMoon metadata is invalid: {err}");
            return Ok(UpdateStatus::ReinstallRequired);
        }
    };

    if let Err(err) = verify_content_hash(install_dir, manifest, &installed.tag) {
        eprintln!("Installed CraftMoon files differ from the manifest: {err}");
        return Ok(UpdateStatus::ReinstallRequired);
    }

    Ok(if installed.tag == manifest.game_version {
        UpdateStatus::UpToDate
    } else {
        UpdateStatus::UpdateAvailable { installed }
    })
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
            set_status(format!("Installing CraftMoon {}...", manifest.game_version));
            install_full_archive(client, install_dir, manifest, set_progress)
        }
        UpdateStatus::ReinstallRequired => {
            set_status(
                "CraftMoon files differ from the published release; reinstalling...".to_string(),
            );
            install_full_archive(client, install_dir, manifest, set_progress)
        }
        UpdateStatus::UpdateAvailable { installed } => {
            set_status(format!(
                "Updating CraftMoon {} -> {}...",
                installed.tag, manifest.game_version
            ));
            try_patch_update(
                client,
                install_dir,
                manifest,
                &installed,
                &mut set_status,
                &mut set_progress,
            )
        }
    }
}

fn install_full_archive(
    client: &Client,
    install_dir: &Path,
    manifest: &Manifest,
    mut set_progress: impl FnMut(u64, u64),
) -> anyhow::Result<()> {
    let archive = manifest.game_archive(CURRENT_PLATFORM)?;
    stage_and_promote(install_dir, |staging_dir| {
        let temp = download_from_mirrors(
            client,
            &manifest.endpoints,
            &archive.name,
            &archive.sha256,
            staging_dir,
            &mut set_progress,
        )?;
        if archive.name.ends_with(".zip") {
            extract_zip(temp.path(), staging_dir)?;
        } else {
            extract_tar_gz(temp.path(), staging_dir)?;
        }
        drop(temp);

        set_linux_game_executable_permission(staging_dir)?;
        verify_content_hash(staging_dir, manifest, &manifest.game_version)?;
        write_version_atomic(staging_dir, &InstalledVersion::new(&manifest.game_version))?;
        Ok(())
    })
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
        "no patch steps from {} to {}",
        installed.tag,
        manifest.game_version
    );

    stage_and_promote(install_dir, |staging_dir| {
        copy_install_to_staging(install_dir, staging_dir)?;

        let mut current_tag = installed.tag.clone();
        verify_content_hash(staging_dir, manifest, &current_tag)?;
        let total_steps = chain.len();
        for (index, (target_tag, patch_name, expected_patch_hash)) in chain.iter().enumerate() {
            let step = index + 1;
            set_status(format!(
                "Downloading CraftMoon patch {step}/{total_steps}: {current_tag} -> {target_tag}..."
            ));
            let temp = download_from_mirrors(
                client,
                &manifest.endpoints,
                patch_name,
                expected_patch_hash,
                staging_dir,
                &mut *set_progress,
            )?;

            set_status(format!(
                "Applying CraftMoon patch {step}/{total_steps}: {current_tag} -> {target_tag}..."
            ));
            let patched_tag = apply_patch_bundle(temp.path(), staging_dir, &current_tag)?;
            drop(temp);
            anyhow::ensure!(
                patched_tag == *target_tag,
                "patch bundle target tag mismatch: expected {target_tag}, got {patched_tag}"
            );
            verify_content_hash(staging_dir, manifest, target_tag)?;
            current_tag = patched_tag;
        }

        anyhow::ensure!(
            current_tag == manifest.game_version,
            "patch chain ended at {current_tag}, expected {}",
            manifest.game_version
        );
        set_linux_game_executable_permission(staging_dir)?;
        write_version_atomic(staging_dir, &InstalledVersion::new(&manifest.game_version))?;
        Ok(())
    })
}

fn stage_and_promote(
    install_dir: &Path,
    operation: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let staging_dir = prepare_staging_dir(install_dir)?;
    if let Err(err) = operation(&staging_dir) {
        if let Err(cleanup_err) = remove_path(&staging_dir) {
            return Err(err.context(format!(
                "failed to clean staged install after update error: {cleanup_err}"
            )));
        }
        return Err(err);
    }
    promote_staging_dir(&staging_dir, install_dir)
}

fn verify_content_hash(
    install_dir: &Path,
    manifest: &Manifest,
    version: &str,
) -> anyhow::Result<()> {
    let actual = hash_directory(install_dir)?;
    let expected = manifest.content_hash(version, CURRENT_PLATFORM)?;
    anyhow::ensure!(
        actual == expected,
        "content hash mismatch for {version}: expected {expected}, got {actual}"
    );
    Ok(())
}

fn prepare_staging_dir(install_dir: &Path) -> anyhow::Result<PathBuf> {
    let staging_dir = sibling_path(install_dir, "staging")?;
    remove_path(&staging_dir)?;
    std::fs::create_dir_all(&staging_dir).with_context(|| {
        format!(
            "failed to create staging directory {}",
            staging_dir.display()
        )
    })?;
    Ok(staging_dir)
}

fn copy_install_to_staging(source: &Path, destination: &Path) -> anyhow::Result<()> {
    for entry in WalkDir::new(source) {
        let entry = entry.with_context(|| format!("failed to walk {}", source.display()))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .with_context(|| format!("failed to make {} relative", entry.path().display()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::copy(entry.path(), &target).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    entry.path().display(),
                    target.display()
                )
            })?;
            std::fs::set_permissions(&target, entry.metadata()?.permissions()).with_context(
                || format!("failed to preserve permissions on {}", target.display()),
            )?;
        } else {
            anyhow::bail!(
                "refusing to stage non-file entry {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn promote_staging_dir(staging_dir: &Path, install_dir: &Path) -> anyhow::Result<()> {
    let backup_dir = sibling_path(install_dir, "previous")?;
    remove_path(&backup_dir)?;

    let had_install = install_dir.exists();
    if had_install {
        std::fs::rename(install_dir, &backup_dir).with_context(|| {
            format!(
                "failed to move current install {} aside",
                install_dir.display()
            )
        })?;
    }

    if let Err(err) = std::fs::rename(staging_dir, install_dir) {
        if had_install {
            std::fs::rename(&backup_dir, install_dir).with_context(|| {
                format!(
                    "failed to restore previous install {} after promotion error: {err}",
                    install_dir.display()
                )
            })?;
        }
        return Err(err).with_context(|| {
            format!(
                "failed to promote staged install {} to {}",
                staging_dir.display(),
                install_dir.display()
            )
        });
    }

    if let Err(err) = remove_path(&backup_dir) {
        eprintln!(
            "Failed to remove previous CraftMoon install {}: {err}",
            backup_dir.display()
        );
    }
    Ok(())
}

fn sibling_path(install_dir: &Path, suffix: &str) -> anyhow::Result<PathBuf> {
    let name = install_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid install directory {}", install_dir.display()))?;
    Ok(install_dir.with_file_name(format!(".{name}.{suffix}")))
}

fn remove_path(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove {}", path.display())),
        Ok(_) => std::fs::remove_file(path)
            .with_context(|| format!("failed to remove {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn build_patch_chain(
    manifest: &Manifest,
    from_tag: &str,
    to_tag: &str,
) -> anyhow::Result<Vec<(String, String, String)>> {
    let platform_suffix = if cfg!(windows) {
        ".patch"
    } else {
        "-linux.patch"
    };
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
            edges.entry(from.to_string()).or_default().push((
                to.to_string(),
                filename.clone(),
                hash.clone(),
            ));
        }
    }

    let mut queue = VecDeque::from([from_tag.to_string()]);
    let mut visited: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    visited.insert(from_tag.to_string(), Vec::new());

    while let Some(current) = queue.pop_front() {
        if current == to_tag {
            return Ok(visited.remove(&current).expect("visited current tag"));
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

    anyhow::bail!("no patch chain found from {from_tag} to {to_tag} in manifest")
}

pub fn download_from_mirrors(
    client: &Client,
    endpoints: &[String],
    filename: &str,
    expected_hash: &str,
    download_dir: &Path,
    progress: &mut impl FnMut(u64, u64),
) -> anyhow::Result<crate::download::TempDownload> {
    let mut last_error = None;

    for (i, endpoint) in endpoints.iter().enumerate() {
        let url = format!("{endpoint}/download/{filename}");

        match download_asset_to_temp(client, &url, filename, 0, download_dir, &mut *progress) {
            Ok(temp) => {
                let actual = hash_file(temp.path())?;
                if actual == expected_hash {
                    return Ok(temp);
                }

                eprintln!(
                    "Endpoint {i} ({endpoint}): hash mismatch for {filename} \
                     (expected {expected_hash}, got {actual})"
                );
                last_error = Some(anyhow::anyhow!(
                    "hash mismatch from endpoint {i} for {filename}"
                ));
            }
            Err(err) => {
                eprintln!("Endpoint {i} ({endpoint}) failed for {filename}: {err}");
                last_error = Some(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no download endpoints available")))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::manifest::Asset;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "craftmoon-launcher-{name}-{}-{sequence}",
            std::process::id()
        ))
    }

    fn manifest(version: &str, hashes: &[(&str, &str)]) -> Manifest {
        let mut game_archives = BTreeMap::new();
        game_archives.insert(
            CURRENT_PLATFORM.to_string(),
            Asset {
                name: "CraftMoon-linux-0.4.tar.gz".to_string(),
                sha256: "0".repeat(64),
            },
        );
        let mut game_content_hashes = BTreeMap::new();
        for (tag, content_hash) in hashes {
            game_content_hashes.insert(
                (*tag).to_string(),
                BTreeMap::from([(CURRENT_PLATFORM.to_string(), (*content_hash).to_string())]),
            );
        }
        Manifest {
            game_version: version.to_string(),
            game_archives,
            game_content_hashes,
            launcher_version: "1.0.0".to_string(),
            launcher_binaries: BTreeMap::from([(
                CURRENT_PLATFORM.to_string(),
                Asset {
                    name: "craftmoon-launcher-linux-1.0.0".to_string(),
                    sha256: "0".repeat(64),
                },
            )]),
            patches: BTreeMap::new(),
            endpoints: Vec::new(),
        }
    }

    fn install(dir: &Path, tag: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("game.bin"), b"game data").unwrap();
        let content_hash = hash_directory(dir).unwrap();
        write_version_atomic(dir, &InstalledVersion::new(tag)).unwrap();
        content_hash
    }

    #[test]
    fn current_matching_install_is_up_to_date() {
        let dir = test_dir("up-to-date");
        let content_hash = install(&dir, "0.4");

        assert!(matches!(
            check_for_update(&manifest("0.4", &[("0.4", &content_hash)]), &dir).unwrap(),
            UpdateStatus::UpToDate
        ));
        remove_path(&dir).unwrap();
    }

    #[test]
    fn current_install_different_from_manifest_requires_reinstall() {
        let dir = test_dir("manifest-mismatch");
        install(&dir, "0.4");
        let published_hash = "0".repeat(64);

        assert!(matches!(
            check_for_update(&manifest("0.4", &[("0.4", &published_hash)]), &dir).unwrap(),
            UpdateStatus::ReinstallRequired
        ));
        remove_path(&dir).unwrap();
    }

    #[test]
    fn modified_older_install_requires_reinstall_not_patch() {
        let dir = test_dir("modified-old");
        let content_hash = install(&dir, "0.3");
        std::fs::write(dir.join("game.bin"), b"modified data").unwrap();

        assert!(matches!(
            check_for_update(&manifest("0.4", &[("0.3", &content_hash)]), &dir).unwrap(),
            UpdateStatus::ReinstallRequired
        ));
        remove_path(&dir).unwrap();
    }

    #[test]
    fn intact_older_install_can_use_patches() {
        let dir = test_dir("old-intact");
        let content_hash = install(&dir, "0.3");

        assert!(matches!(
            check_for_update(&manifest("0.4", &[("0.3", &content_hash)]), &dir).unwrap(),
            UpdateStatus::UpdateAvailable { .. }
        ));
        remove_path(&dir).unwrap();
    }

    #[test]
    fn unlisted_installed_version_requires_reinstall() {
        let dir = test_dir("unlisted-version");
        install(&dir, "0.2");
        let published_hash = "0".repeat(64);

        assert!(matches!(
            check_for_update(&manifest("0.4", &[("0.4", &published_hash)]), &dir).unwrap(),
            UpdateStatus::ReinstallRequired
        ));
        remove_path(&dir).unwrap();
    }
}
