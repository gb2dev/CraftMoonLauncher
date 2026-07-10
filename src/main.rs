// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use slint::Weak;

mod download;
mod extract;
mod hash;
mod http;
mod make_patch;
mod manifest;
mod patch;
mod platform;
mod updater;
mod version;

use http::http_client;
use make_patch::{PatchPlatform, make_archive, make_patch};
use manifest::{Manifest, fetch_manifest};
use platform::{CURRENT_PLATFORM, GAME_EXECUTABLE_NAME, make_executable};
use updater::{UpdateStatus, check_for_update, download_from_mirrors, perform_update};

slint::include_modules!();

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
    /// Create Windows and/or Linux patch bundles from extracted old/new game directories.
    MakePatch {
        /// Path to previous version's extracted game files.
        #[arg(long)]
        old_dir: PathBuf,

        /// Path to new version's extracted game files.
        #[arg(long)]
        new_dir: PathBuf,

        /// Previous release tag, e.g. v0.2.
        #[arg(long)]
        from_tag: String,

        /// New release tag, e.g. v0.3.
        #[arg(long)]
        to_tag: String,

        /// Directory to write patch bundles into.
        #[arg(long)]
        out_dir: PathBuf,

        /// Target platform (windows, linux, or both). Default: both.
        #[arg(long, default_value = "both")]
        platform: String,
    },

    /// Create a full game archive (ZIP on Windows, tar.gz on Linux).
    MakeArchive {
        /// Path to the extracted game directory to archive.
        #[arg(long)]
        dir: PathBuf,

        /// Game release tag, e.g. 0.5.
        #[arg(long)]
        version: String,

        /// Directory to write the archive into.
        #[arg(long)]
        out_dir: PathBuf,

        /// Target platform (windows or linux). Defaults to current platform.
        #[arg(long)]
        platform: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    match args.command {
        Some(Commands::MakePatch {
            old_dir,
            new_dir,
            from_tag,
            to_tag,
            out_dir,
            platform,
        }) => {
            let platform: PatchPlatform = platform
                .parse()
                .map_err(|e: String| anyhow::anyhow!("invalid --platform value: {e}"))?;
            make_patch(old_dir, new_dir, &from_tag, &to_tag, out_dir, platform)?;
            return Ok(());
        }
        Some(Commands::MakeArchive {
            dir,
            version,
            out_dir,
            platform,
        }) => {
            let platform = match platform {
                Some(p) => Some(
                    p.parse::<PatchPlatform>()
                        .map_err(|e| anyhow::anyhow!("invalid --platform value: {e}"))?,
                ),
                None => None,
            };
            make_archive(dir, &version, out_dir, platform)?;
            return Ok(());
        }
        None => {}
    }

    let install_dir = install_dir()?;
    updater::recover_install(&install_dir)?;
    std::fs::create_dir_all(&install_dir).with_context(|| {
        format!(
            "failed to create install directory {}",
            install_dir.display()
        )
    })?;
    let game_path = install_dir.join(GAME_EXECUTABLE_NAME);

    let ui = AppWindow::new()?;

    let game_path_for_button = game_path.clone();
    ui.on_launch_game(move || {
        launch_game(game_path_for_button.clone());
    });

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        let client = match http_client() {
            Ok(client) => client,
            Err(err) => {
                let message = format!("Failed to create HTTP client: {err}");
                set_error_status(&ui_weak, message);
                return;
            }
        };
        set_status(&ui_weak, "Checking for updates...");
        let manifest = match fetch_manifest(&client) {
            Ok(manifest) => manifest,
            Err(err) => {
                let message = format!("Failed to fetch update manifest: {err}");
                set_error_status(&ui_weak, &message);
                if game_path.exists() {
                    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.set_progress(0.0);
                        ui.set_show_launch_button(true);
                        ui.set_status_text("Could not check for updates. Launch anyway?".into());
                    });
                } else {
                    set_error_status(&ui_weak, message);
                }
                return;
            }
        };

        if !args.no_self_update {
            update_launcher(&client, &manifest, &ui_weak);
        }

        let mut game_update_failed = false;
        if !args.no_game_update {
            set_status(&ui_weak, "Checking CraftMoon for updates...");
            match check_for_update(&manifest, &install_dir) {
                Ok(status) => {
                    describe_update_status(&ui_weak, &manifest, &status);
                    let status_ui = ui_weak.clone();
                    let progress_ui = ui_weak.clone();
                    if let Err(err) = perform_update(
                        &client,
                        &install_dir,
                        &manifest,
                        status,
                        move |message| set_status(&status_ui, message),
                        move |downloaded, total| {
                            set_download_progress(&progress_ui, downloaded, total)
                        },
                    ) {
                        let message = format!("CraftMoon update failed: {err}");
                        set_error_status(&ui_weak, message);
                        game_update_failed = true;
                    }
                }
                Err(err) => {
                    let message = format!("Failed to check CraftMoon updates: {err}");
                    set_error_status(&ui_weak, message);
                    game_update_failed = true;
                }
            }
        }

        if game_update_failed {
            if game_path.exists() {
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_progress(0.0);
                    ui.set_show_launch_button(true);
                    ui.set_status_text("Update failed. Launch anyway?".into());
                });
            }
        } else if game_path.exists() {
            set_status(&ui_weak, "Launching CraftMoon...");
            launch_game(game_path);
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
fn install_dir() -> anyhow::Result<PathBuf> {
    dirs::data_local_dir()
        .map(|data_dir| data_dir.join("CraftMoon"))
        .ok_or_else(|| anyhow::anyhow!("failed to find local user data directory"))
}

#[cfg(not(windows))]
fn install_dir() -> anyhow::Result<PathBuf> {
    dirs::data_dir()
        .map(|data_dir| data_dir.join("CraftMoon"))
        .ok_or_else(|| anyhow::anyhow!("failed to find user data directory"))
}

fn update_launcher(
    client: &reqwest::blocking::Client,
    manifest: &Manifest,
    ui_weak: &Weak<AppWindow>,
) {
    set_status(ui_weak, "Checking CraftMoon Launcher for updates...");

    if !launcher_update_available(&manifest.launcher_version) {
        return;
    }

    let launcher_asset = match manifest.launcher_binary(CURRENT_PLATFORM) {
        Ok(asset) => asset,
        Err(err) => {
            eprintln!("Manifest does not provide a launcher update for this platform: {err}");
            return;
        }
    };

    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Failed to get current executable path: {err}");
            return;
        }
    };
    let download_dir = current_exe
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    set_status(
        ui_weak,
        format!(
            "Downloading CraftMoon Launcher {}...",
            manifest.launcher_version
        ),
    );
    let temp = match download_from_mirrors(
        client,
        &manifest.endpoints,
        &launcher_asset.name,
        &launcher_asset.sha256,
        &download_dir,
        &mut |downloaded, total| set_download_progress(ui_weak, downloaded, total),
    ) {
        Ok(temp) => temp,
        Err(err) => {
            eprintln!("Failed to download CraftMoon Launcher update: {err}");
            return;
        }
    };

    if let Err(err) = make_executable(temp.path()) {
        eprintln!("Failed to mark CraftMoon Launcher update executable: {err}");
        return;
    }

    let saved_exe = current_exe.clone();

    if let Err(err) = self_replace::self_replace(temp.path()) {
        eprintln!("Failed to replace current launcher executable: {err}");
        return;
    }

    set_status(ui_weak, "Launcher updated. Restarting...");
    drop(temp);
    restart_program(&saved_exe);
}

fn launcher_update_available(latest_version: &str) -> bool {
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION must be valid semver");
    match semver::Version::parse(latest_version) {
        Ok(latest) => latest > current,
        Err(err) => {
            eprintln!("Manifest has an invalid launcher version {latest_version:?}: {err}");
            false
        }
    }
}

fn describe_update_status(ui_weak: &Weak<AppWindow>, manifest: &Manifest, status: &UpdateStatus) {
    match status {
        UpdateStatus::FirstInstall => {
            set_status(
                ui_weak,
                format!("CraftMoon {} is available.", manifest.game_version),
            );
        }
        UpdateStatus::ReinstallRequired => {
            set_status(
                ui_weak,
                format!(
                    "CraftMoon install differs from the published {} release.",
                    manifest.game_version
                ),
            );
        }
        UpdateStatus::UpdateAvailable { installed } => {
            set_status(
                ui_weak,
                format!(
                    "CraftMoon update available: {} -> {}.",
                    installed.tag, manifest.game_version
                ),
            );
        }
        UpdateStatus::UpToDate => {
            set_status(
                ui_weak,
                format!("CraftMoon {} is up to date.", manifest.game_version),
            );
        }
    }
}

fn set_status(ui_weak: &Weak<AppWindow>, message: impl Into<String>) {
    let message = message.into();
    println!("{message}");
    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
        ui.set_status_text(message.into());
    });
}

fn set_error_status(ui_weak: &Weak<AppWindow>, message: impl Into<String>) {
    let message = message.into();
    eprintln!("{message}");
    let _ = ui_weak.upgrade_in_event_loop(move |ui| {
        ui.set_status_text(message.into());
    });
}

fn set_download_progress(ui_weak: &Weak<AppWindow>, downloaded: u64, total: u64) {
    if total > 0 {
        println!("Downloaded {downloaded}/{total} bytes");
        let progress = (downloaded as f64 / total as f64).clamp(0.0, 1.0) as f32;
        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
            ui.set_progress(progress);
        });
    } else {
        println!("Downloaded {downloaded} bytes");
    }
}

fn launch_game(game_path: PathBuf) {
    if let Err(err) = std::process::Command::new(&game_path).spawn() {
        eprintln!("Failed to launch game {}: {err}", game_path.display());
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}

fn restart_program(exe_path: &std::path::Path) {
    if let Err(err) = std::process::Command::new(exe_path)
        .arg("--no-self-update")
        .spawn()
    {
        eprintln!(
            "Failed to restart launcher at {}: {err}",
            exe_path.display()
        );
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}
