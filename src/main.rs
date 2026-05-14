// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::error::Error;
use std::fmt::Debug;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slint::Weak;

slint::include_modules!();

const USER_AGENT_VALUE: &str = "crafmoon-launcher";
const LAUNCHER_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "craftmoon-launcher.exe"
} else {
    "craftmoon-launcher-linux"
};
const GAME_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "CraftMoon.exe"
} else {
    "CraftMoon-linux.x86_64"
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Do not update the launcher
    #[arg(long)]
    no_self_update: bool,

    /// Do not update the game
    #[arg(long)]
    no_game_update: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a binary diff patch between two files
    Diff {
        /// "Old" file path
        file_a: PathBuf,
        /// "New" file path
        file_b: PathBuf,
        /// Patch file path
        patch_file: PathBuf,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct Release {
    id: u64,
    body: String,
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Asset {
    id: u64,
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    if let Some(Commands::Diff {
        file_a,
        file_b,
        patch_file,
    }) = args.command
    {
        diff_files(file_a, file_b, patch_file)?;
        return Ok(());
    }

    let install_dir = install_dir()?;
    std::fs::create_dir_all(&install_dir)?;
    let game_path = install_dir.join(GAME_EXECUTABLE_NAME);

    let ui = AppWindow::new()?;

    let game_path_for_launch = game_path.clone();
    ui.on_launch_game(move || {
        launch_game(&game_path_for_launch);
    });

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        if !args.no_self_update {
            let client = reqwest::blocking::Client::new();
            let (releases, is_running_latest) =
                check_for_updated_release(&client, &ui_weak, "CraftMoonLauncher", true, &None);
            if !is_running_latest {
                update_launcher(&client, &ui_weak, &releases[0]);
            }
        }

        let mut game_update_failed = false;

        if !args.no_game_update {
            let client = reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .unwrap();
            let exe_file = match std::fs::read(&game_path) {
                Ok(exe_file) => Some(exe_file),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => {
                    eprintln!("Failed to read game executable: {err}");
                    None
                }
            };
            let exe_hash = if let Some(exe_file) = exe_file {
                Some(format!("sha256:{}", hex_digest(Sha256::digest(exe_file))))
            } else {
                None
            };
            let (releases, is_running_latest) =
                check_for_updated_release(&client, &ui_weak, "CraftMoon", false, &exe_hash);
            if !is_running_latest {
                game_update_failed =
                    !update_game(&client, &ui_weak, releases, &game_path, exe_hash);
            }
        }

        if game_update_failed && game_path.exists() {
            ui_weak
                .upgrade_in_event_loop(move |ui| {
                    ui.set_show_launch_button(true);
                    ui.set_status_text("Update failed. Launch anyway?".into());
                })
                .unwrap();
        } else if game_path.exists() {
            launch_game(&game_path);
        } else {
            set_status(
                &ui_weak,
                "No CraftMoon build is installed for this platform.",
            );
        }
    });

    ui.run()?;

    Ok(())
}

#[cfg(windows)]
fn install_dir() -> Result<PathBuf, Box<dyn Error>> {
    dirs::data_local_dir()
        .map(|data_dir| data_dir.join("CraftMoon"))
        .ok_or_else(|| "failed to find local user data directory".into())
}

#[cfg(not(windows))]
fn install_dir() -> Result<PathBuf, Box<dyn Error>> {
    dirs::data_dir()
        .map(|data_dir| data_dir.join("CraftMoon"))
        .ok_or_else(|| "failed to find user data directory".into())
}

fn update_launcher(
    client: &reqwest::blocking::Client,
    ui_weak: &Weak<AppWindow>,
    release: &Release,
) {
    let Some(exe_asset) = release
        .assets
        .iter()
        .find(|&asset| is_platform_executable_asset(asset, LAUNCHER_EXECUTABLE_NAME))
    else {
        eprintln!("No launcher release asset found for this platform.");
        return;
    };

    let current_exe_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Failed to get current executable path: {err}");
            return;
        }
    };
    let Some(current_exe_name) = current_exe_path.file_name() else {
        eprintln!("Failed to get current executable file name");
        return;
    };

    let temp_dir = std::env::temp_dir();
    let destination = temp_dir.join(current_exe_name);

    let message = format!("Downloading CraftMoonLauncher update...").into();
    ui_weak
        .upgrade_in_event_loop(move |ui| {
            ui.set_status_text(message);
        })
        .unwrap();

    if let Err(err) = download_file(
        client,
        &exe_asset.browser_download_url,
        &destination,
        ui_weak,
    ) {
        eprintln!("Failed to download CraftMoonLauncher update: {err}");
        return;
    }

    if let Err(err) = make_executable(&destination) {
        eprintln!("Failed to mark CraftMoonLauncher update executable: {err}");
        return;
    }

    if let Err(err) = self_replace::self_replace(&destination) {
        eprintln!("Failed to replace current exe: {err}");
        return;
    };

    if let Err(err) = std::fs::remove_file(&destination) {
        eprintln!("Failed to delete exe: {err}");
        return;
    };

    restart_program();
}

fn update_game(
    client: &reqwest::blocking::Client,
    ui_weak: &Weak<AppWindow>,
    releases: Vec<Release>,
    game_path: &Path,
    exe_hash: Option<String>,
) -> bool {
    let installed_release_index = releases.iter().position(|release| {
        release
            .assets
            .iter()
            .find(|&asset| is_platform_executable_asset(asset, GAME_EXECUTABLE_NAME))
            .is_some_and(|asset| asset.digest == exe_hash)
    });

    let mut reinstall_game = installed_release_index.is_none();

    if !reinstall_game {
        let patch_assets: Vec<Option<&Asset>> = releases
            .iter()
            .rev()
            .skip(releases.len() - installed_release_index.unwrap_or_default())
            .map(|release| {
                release
                    .assets
                    .iter()
                    .find(|&asset| is_platform_patch_asset(asset))
            })
            .collect();

        let temp_dir = std::env::temp_dir();

        for (index, asset) in patch_assets.iter().enumerate() {
            let Some(asset) = asset else {
                reinstall_game = true;
                break;
            };

            let message = format!(
                "Downloading CraftMoon update ({}/{})...",
                index + 1,
                patch_assets.len()
            )
            .into();
            ui_weak
                .upgrade_in_event_loop(move |ui| {
                    ui.set_status_text(message);
                })
                .unwrap();

            let destination = temp_dir.join(&asset.name);

            if let Err(err) =
                download_file(client, &asset.browser_download_url, &destination, ui_weak)
            {
                eprintln!("Failed to download CraftMoon update: {err}");
                reinstall_game = true;
                break;
            }

            if let Err(err) = patch_file(game_path, &destination) {
                eprintln!("Failed to update CraftMoon: {err}");
                reinstall_game = true;
                break;
            }

            if let Err(err) = std::fs::remove_file(destination) {
                eprintln!("Failed to remove CraftMoon update file: {err}");
            }
        }
    }

    if reinstall_game {
        let is_first_install = game_path
            .read_dir()
            .map_or_else(|_| true, |mut it| it.next().is_none());

        let message = if is_first_install {
            "Installing CraftMoon...".into()
        } else {
            "Reinstalling CraftMoon...".into()
        };
        ui_weak
            .upgrade_in_event_loop(move |ui| {
                ui.set_status_text(message);
            })
            .unwrap();

        let latest_release = &releases[0];
        let Some(exe_asset) = latest_release
            .assets
            .iter()
            .find(|&asset| is_platform_executable_asset(asset, GAME_EXECUTABLE_NAME))
        else {
            let message = "No game release asset found for this platform.";
            eprintln!("{message}");
            set_status(ui_weak, message);
            return false;
        };
        if let Err(err) = download_file(client, &exe_asset.browser_download_url, game_path, ui_weak)
        {
            eprintln!("Failed to download CraftMoon: {err}");
            return false;
        }
        if let Err(err) = make_executable(game_path) {
            eprintln!("Failed to mark CraftMoon executable: {err}");
            return false;
        }
    }

    true
}

fn is_platform_executable_asset(asset: &Asset, executable_name: &str) -> bool {
    asset.name == executable_name
}

fn is_platform_patch_asset(asset: &Asset) -> bool {
    let lower_name = asset.name.to_ascii_lowercase();
    if !lower_name.ends_with(".patch") {
        return false;
    }

    if cfg!(windows) {
        return !lower_name.contains("linux")
            && !lower_name.contains("macos")
            && !lower_name.contains("darwin");
    }

    lower_name.contains("linux")
}

fn make_executable(path: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = path.as_ref();
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        std::fs::set_permissions(path, permissions)?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn set_status(ui_weak: &Weak<AppWindow>, message: impl Into<slint::SharedString>) {
    let message = message.into();
    ui_weak
        .upgrade_in_event_loop(move |ui| {
            ui.set_status_text(message);
        })
        .unwrap();
}

fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    destination: impl AsRef<Path> + Debug,
    ui_weak: &Weak<AppWindow>,
) -> Result<(), Box<dyn Error>> {
    let mut response = client
        .get(url)
        .header("User-Agent", USER_AGENT_VALUE)
        .send()?;

    if !response.status().is_success() {
        return Err(format!("Status {}", response.status()).into());
    }

    let total_size = response.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;
    let mut last_reported_progress = -1.0;

    let mut file = std::fs::File::create(&destination)?;
    let mut buffer = [0u8; 8192];

    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;

        if total_size > 0 {
            let progress = downloaded as f64 / total_size as f64;
            if progress - last_reported_progress >= 0.01 {
                last_reported_progress = progress;
                let ui = ui_weak.clone();
                ui.upgrade_in_event_loop(move |ui| {
                    ui.set_progress(progress as f32);
                })
                .unwrap();
            }
        }
    }

    Ok(())
}

fn check_for_updated_release(
    client: &reqwest::blocking::Client,
    ui_weak: &Weak<AppWindow>,
    project_name: &str,
    self_update: bool,
    exe_hash: &Option<String>,
) -> (Vec<Release>, bool) {
    let message = format!("Checking {project_name} for updates...").into();
    ui_weak
        .upgrade_in_event_loop(move |ui| {
            ui.set_status_text(message);
        })
        .unwrap();
    let releases = match get_releases(client, project_name) {
        Ok(releases) => releases,
        Err(err) => {
            let message = format!("{err}").into();
            eprintln!("Failed to get {project_name} releases: {err}");
            ui_weak
                .upgrade_in_event_loop(move |ui| {
                    ui.set_status_text(message);
                })
                .unwrap();
            return (Vec::new(), true);
        }
    };
    if releases.is_empty() {
        eprintln!("No {project_name} releases found");
        return (releases, true);
    }
    let latest_release = &releases[0];
    let is_running_latest = if self_update {
        env!("CARGO_PKG_VERSION") == &latest_release.tag_name
    } else if let Some(exe_hash) = exe_hash {
        latest_release
            .assets
            .iter()
            .find(|&asset| is_platform_executable_asset(asset, GAME_EXECUTABLE_NAME))
            .is_some_and(|asset| asset.digest.as_deref() == Some(exe_hash))
    } else {
        false
    };

    (releases, is_running_latest)
}

fn get_releases(
    client: &reqwest::blocking::Client,
    project_name: &str,
) -> Result<Vec<Release>, Box<dyn Error>> {
    let url = format!("https://api.github.com/repos/gb2dev/{project_name}/releases");
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", USER_AGENT_VALUE)
        .send()?;
    if !response.status().is_success() {
        return Err(format!("Failed to fetch releases: {}", response.status()).into());
    }
    let releases: Vec<Release> = response.json()?;
    if releases.is_empty() {
        return Err("No releases found".into());
    }

    Ok(releases)
}

fn launch_game(game_path: &Path) {
    if let Err(err) = std::process::Command::new(game_path).spawn() {
        eprintln!("Failed to launch game: {err}");
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}

fn restart_program() {
    let current_exe = match std::env::current_exe() {
        Ok(current_exe) => current_exe,
        Err(err) => {
            eprintln!("failed to get current exe path: {err}");
            return;
        }
    };
    if let Err(err) = std::process::Command::new(current_exe)
        .arg("--no-self-update")
        .spawn()
    {
        eprintln!("Failed to restart program: {err}");
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}

fn patch_file(file: impl AsRef<Path>, patch_file: impl AsRef<Path>) -> std::io::Result<()> {
    let old = std::fs::read(&file)?;
    let compressed_patch = std::fs::read(patch_file)?;

    let mut patch = Vec::new();
    let mut decompressor = bzip2::read::BzDecoder::new(compressed_patch.as_slice());
    decompressor.read_to_end(&mut patch)?;

    let mut new = Vec::new();
    bsdiff::patch(&old, &mut patch.as_slice(), &mut new)?;
    std::fs::write(file, &new)
}

fn diff_files(
    file_a: impl AsRef<Path>,
    file_b: impl AsRef<Path>,
    patch_file: impl AsRef<Path>,
) -> std::io::Result<()> {
    let old = std::fs::read(file_a)?;
    let new = std::fs::read(file_b)?;
    let mut patch = Vec::new();

    bsdiff::diff(&old, &new, &mut patch)?;

    let mut compressed_patch = Vec::new();
    {
        let mut compressor =
            bzip2::write::BzEncoder::new(&mut compressed_patch, bzip2::Compression::best());
        compressor.write_all(&patch)?;
        compressor.finish()?;
    }

    std::fs::write(patch_file, &compressed_patch)
}
