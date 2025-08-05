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
use windows::Win32::UI::Shell::{FOLDERID_UserProgramFiles, SHGetKnownFolderPath};

slint::include_modules!();

const USER_AGENT_VALUE: &str = "crafmoon-launcher";

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

    let programs_path_pwstr =
        unsafe { SHGetKnownFolderPath(&FOLDERID_UserProgramFiles, Default::default(), None) }?;
    let programs_path_str = unsafe { programs_path_pwstr.to_string() }? + "\\CraftMoon";
    std::fs::create_dir_all(&programs_path_str)?;

    let ui = AppWindow::new()?;

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

        let exe_path_str = programs_path_str + "\\CraftMoon.exe";

        if !args.no_game_update {
            let client = reqwest::blocking::Client::builder()
                .timeout(None)
                .build()
                .unwrap();
            let exe_file = std::fs::read(&exe_path_str)
                .inspect_err(|err| eprintln!("Failed to read exe file: {err}"))
                .ok();
            let exe_hash = if let Some(exe_file) = exe_file {
                Some(format!("sha256:{:x}", Sha256::digest(exe_file)))
            } else {
                None
            };
            let (releases, is_running_latest) =
                check_for_updated_release(&client, &ui_weak, "CraftMoon", false, &exe_hash);
            if !is_running_latest {
                update_game(&client, &ui_weak, releases, &exe_path_str, exe_hash);
            }
        }

        launch_game(&exe_path_str);
    });

    ui.run()?;

    Ok(())
}

fn update_launcher(
    client: &reqwest::blocking::Client,
    ui_weak: &Weak<AppWindow>,
    release: &Release,
) {
    let Some(exe_asset) = release
        .assets
        .iter()
        .find(|&asset| asset.name.ends_with(".exe"))
    else {
        eprintln!("No exe release asset found");
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

    if let Err(err) = download_file(client, &exe_asset.browser_download_url, &destination) {
        eprintln!("Failed to download CraftMoonLauncher update: {err}");
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
    exe_path_str: &str,
    exe_hash: Option<String>,
) {
    let installed_release_index = releases.iter().position(|release| {
        release
            .assets
            .iter()
            .find(|&asset| asset.name.ends_with(".exe"))
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
                    .find(|&asset| asset.name.ends_with(".patch"))
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

            if let Err(err) = download_file(client, &asset.browser_download_url, &destination) {
                eprintln!("Failed to download CraftMoon update: {err}");
                reinstall_game = true;
                break;
            }

            if let Err(err) = patch_file(exe_path_str, &destination) {
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
        let message = format!("Reinstalling CraftMoon...").into();
        ui_weak
            .upgrade_in_event_loop(move |ui| {
                ui.set_status_text(message);
            })
            .unwrap();

        let latest_release = &releases[0];
        let Some(exe_asset) = latest_release
            .assets
            .iter()
            .find(|&asset| asset.name.ends_with(".exe"))
        else {
            eprintln!("No exe release asset found");
            return;
        };
        if let Err(err) = download_file(client, &exe_asset.browser_download_url, exe_path_str) {
            eprintln!("Failed to download CraftMoon: {err}");
            return;
        }
    }
}

fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    destination: impl AsRef<Path> + Debug,
) -> Result<(), Box<dyn Error>> {
    let file_response = client
        .get(url)
        .header("User-Agent", USER_AGENT_VALUE)
        .send()?;

    if !file_response.status().is_success() {
        return Err(format!("Status {}", file_response.status()).into());
    }

    let content = file_response.bytes()?;

    let mut file = std::fs::File::create(destination)?;
    file.write_all(&content)?;

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
            .find(|&asset| asset.name.ends_with(".exe"))
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

fn launch_game(exe_path_str: &str) {
    if let Err(err) = std::process::Command::new(exe_path_str).spawn() {
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
