use std::path::Path;

pub const LAUNCHER_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "craftmoon-launcher.exe"
} else {
    "craftmoon-launcher-linux"
};

pub const GAME_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "CraftMoon.exe"
} else {
    "CraftMoon-linux.x86_64"
};

pub const WINDOWS_ARCHIVE_NAME: &str = "CraftMoon-windows.zip";
pub const LINUX_ARCHIVE_NAME: &str = "CraftMoon-linux.tar.gz";

pub const FULL_ARCHIVE_NAME: &str = if cfg!(windows) {
    WINDOWS_ARCHIVE_NAME
} else {
    LINUX_ARCHIVE_NAME
};

pub fn strip_leading_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
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
