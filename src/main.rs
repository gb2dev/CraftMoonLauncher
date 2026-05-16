// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use slint::Weak;

mod download;
mod extract;
mod github;
mod hash;
mod make_patch;
mod patch;
mod platform;
mod updater;
mod version;

use github::{LAUNCHER_REPO, fetch_latest_release_for_repo, github_client};
use make_patch::make_patch;
use platform::{GAME_EXECUTABLE_NAME, LAUNCHER_EXECUTABLE_NAME, make_executable, strip_leading_v};
use updater::{UpdateStatus, check_for_update, perform_update};

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
    /// Create Windows and Linux patch bundles from extracted old/new game directories.
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
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    if let Some(Commands::MakePatch {
        old_dir,
        new_dir,
        from_tag,
        to_tag,
        out_dir,
    }) = args.command
    {
        make_patch(old_dir, new_dir, &from_tag, &to_tag, out_dir)?;
        return Ok(());
    }

    let install_dir = install_dir()?;
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
        let client = match github_client() {
            Ok(client) => client,
            Err(err) => {
                let message = format!("Failed to create HTTP client: {err}");
                eprintln!("{message}");
                set_status(&ui_weak, message);
                return;
            }
        };

        if !args.no_self_update {
            update_launcher(&client, &ui_weak);
        }

        let mut game_update_failed = false;
        if !args.no_game_update {
            set_status(&ui_weak, "Checking CraftMoon for updates...");
            match check_for_update(&client, &install_dir) {
                Ok(status) => {
                    describe_update_status(&ui_weak, &status);
                    let status_ui = ui_weak.clone();
                    let progress_ui = ui_weak.clone();
                    if let Err(err) = perform_update(
                        &client,
                        &install_dir,
                        status,
                        move |message| set_status(&status_ui, message),
                        move |downloaded, total| {
                            set_download_progress(&progress_ui, downloaded, total)
                        },
                    ) {
                        let message = format!("CraftMoon update failed: {err}");
                        eprintln!("{message}");
                        set_status(&ui_weak, message);
                        game_update_failed = true;
                    }
                }
                Err(err) => {
                    let message = format!("Failed to check CraftMoon updates: {err}");
                    eprintln!("{message}");
                    set_status(&ui_weak, message);
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

fn update_launcher(client: &reqwest::blocking::Client, ui_weak: &Weak<AppWindow>) {
    set_status(ui_weak, "Checking CraftMoon Launcher for updates...");

    let latest = match fetch_latest_release_for_repo(client, LAUNCHER_REPO) {
        Ok(latest) => latest,
        Err(err) => {
            eprintln!("Failed to check CraftMoon Launcher updates: {err}");
            return;
        }
    };

    if launcher_version_matches(&latest.tag_name) {
        return;
    }

    let Some(asset) = latest
        .assets
        .iter()
        .find(|asset| asset.name == LAUNCHER_EXECUTABLE_NAME)
    else {
        eprintln!(
            "No CraftMoon Launcher release asset named {} found in {}.",
            LAUNCHER_EXECUTABLE_NAME, latest.tag_name
        );
        return;
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
        format!("Downloading CraftMoon Launcher {}...", latest.tag_name),
    );
    let temp = match download::download_asset_to_temp(
        client,
        &asset.browser_download_url,
        &asset.name,
        asset.size,
        &download_dir,
        |downloaded, total| set_download_progress(ui_weak, downloaded, total),
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

    if let Err(err) = self_replace::self_replace(temp.path()) {
        eprintln!("Failed to replace current launcher executable: {err}");
        return;
    }

    set_status(ui_weak, "Launcher updated. Restarting...");
    drop(temp);
    restart_program();
}

fn launcher_version_matches(latest_tag: &str) -> bool {
    latest_tag == env!("CARGO_PKG_VERSION")
        || strip_leading_v(latest_tag) == env!("CARGO_PKG_VERSION")
}

fn describe_update_status(ui_weak: &Weak<AppWindow>, status: &UpdateStatus) {
    match status {
        UpdateStatus::FirstInstall { latest } => {
            set_status(
                ui_weak,
                format!("CraftMoon {} is available.", latest.tag_name),
            );
        }
        UpdateStatus::CorruptInstall { latest } => {
            set_status(
                ui_weak,
                format!(
                    "CraftMoon install is corrupted; latest is {}.",
                    latest.tag_name
                ),
            );
        }
        UpdateStatus::UpdateAvailable { latest, installed } => {
            set_status(
                ui_weak,
                format!(
                    "CraftMoon update available: {} -> {}.",
                    installed.tag, latest.tag_name
                ),
            );
        }
        UpdateStatus::UpToDate { latest, installed } => {
            set_status(
                ui_weak,
                format!("CraftMoon {} is up to date.", latest.tag_name),
            );
            let _ = installed;
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
        eprintln!("Failed to restart launcher: {err}");
        std::process::exit(1);
    } else {
        std::process::exit(0);
    }
}
