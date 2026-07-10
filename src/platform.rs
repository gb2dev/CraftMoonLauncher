use std::path::Path;

pub const WINDOWS_PLATFORM: &str = "windows";
pub const LINUX_PLATFORM: &str = "linux";

pub const CURRENT_PLATFORM: &str = if cfg!(windows) {
    WINDOWS_PLATFORM
} else {
    LINUX_PLATFORM
};

pub const GAME_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "CraftMoon.exe"
} else {
    "CraftMoon-linux.x86_64"
};

pub fn game_archive_asset_name(platform: &str, version: &str) -> anyhow::Result<String> {
    match platform {
        WINDOWS_PLATFORM => Ok(format!("CraftMoon-windows-{version}.zip")),
        LINUX_PLATFORM => Ok(format!("CraftMoon-linux-{version}.tar.gz")),
        _ => anyhow::bail!("unsupported platform {platform}"),
    }
}

pub fn launcher_asset_name(platform: &str, version: &str) -> anyhow::Result<String> {
    match platform {
        WINDOWS_PLATFORM => Ok(format!("craftmoon-launcher-windows-{version}.exe")),
        LINUX_PLATFORM => Ok(format!("craftmoon-launcher-linux-{version}")),
        _ => anyhow::bail!("unsupported platform {platform}"),
    }
}

pub fn set_linux_game_executable_permission(install_dir: impl AsRef<Path>) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let executable = install_dir.as_ref().join(GAME_EXECUTABLE_NAME);
        if executable.exists() {
            let permissions = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&executable, permissions)?;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = install_dir;
    }

    Ok(())
}

pub fn make_executable(path: impl AsRef<Path>) -> std::io::Result<()> {
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
