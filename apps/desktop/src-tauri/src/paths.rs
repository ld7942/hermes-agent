//! Profile-aware Hermes paths.
//!
//! Mirrors `hermes_constants.get_hermes_home()` on the Python side: an explicit
//! `HERMES_HOME` override wins, otherwise the platform default. Nothing here
//! hardcodes an install location — the desktop shell must work with a Hermes
//! tree living wherever the user (or the installer) put it.

use std::path::{Path, PathBuf};

/// Root of the user's Hermes state: `config.yaml`, `.env`, `logs/`, `venv/`.
pub fn hermes_home() -> PathBuf {
    if let Some(raw) = std::env::var_os("HERMES_HOME") {
        let trimmed = raw.to_string_lossy().trim().to_string();

        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    platform_default_hermes_home()
}

/// The platform default, taking the host OS as data so it is testable on any
/// host. Mirrors `hermes_constants._get_platform_default_hermes_home()` exactly:
/// Windows defaults to `%LOCALAPPDATA%\hermes` (falling back to
/// `~\AppData\Local\hermes` when `LOCALAPPDATA` is unset); everything else to
/// `~/.hermes`.
fn platform_default_hermes_home_for(home: &Path, local_appdata: Option<&str>, is_windows: bool) -> PathBuf {
    if is_windows {
        let base = local_appdata
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"));

        base.join("hermes")
    } else {
        home.join(".hermes")
    }
}

pub fn platform_default_hermes_home() -> PathBuf {
    let local_appdata = std::env::var_os("LOCALAPPDATA").map(|value| value.to_string_lossy().to_string());

    platform_default_hermes_home_for(&home_dir(), local_appdata.as_deref(), cfg!(windows))
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn logs_dir() -> PathBuf {
    hermes_home().join("logs")
}

/// The desktop shell's own log. The Electron main process writes the same file
/// (`hermes_logging.py` documents the layout: agent.log INFO+, errors.log
/// WARNING+, plus desktop.log for the shell itself).
pub fn desktop_log_path() -> PathBuf {
    logs_dir().join("desktop.log")
}

pub fn venv_root() -> PathBuf {
    std::env::var_os("HERMES_VENV")
        .map(PathBuf::from)
        .unwrap_or_else(|| hermes_home().join("venv"))
}

/// `<venv>/bin` on POSIX, `<venv>/Scripts` on Windows.
pub fn venv_bin_dir() -> PathBuf {
    let root = venv_root();

    if cfg!(windows) {
        root.join("Scripts")
    } else {
        root.join("bin")
    }
}

/// Hermes-managed Node directories, leading with the layout native to this
/// platform. `scripts/install.ps1` unpacks portable Node straight into
/// `%LOCALAPPDATA%\hermes\node` (no `bin\`); the POSIX installer uses
/// `$HERMES_HOME/node/bin`. Both are emitted so migrated installs resolve —
/// same rule as `iter_hermes_node_dirs()` in hermes_constants.py.
pub fn hermes_node_path_entries() -> Vec<PathBuf> {
    let root = hermes_home().join("node");
    let bin = root.join("bin");

    if cfg!(windows) {
        vec![root, bin]
    } else {
        vec![bin, root]
    }
}

/// POSIX applications launched from Finder/Dock inherit only
/// `/usr/bin:/bin:/usr/sbin:/sbin`, which misses Homebrew and user-installed
/// CLIs. Mirrors POSIX_SANE_PATH_ENTRIES in `electron/backend-env.ts`.
pub const POSIX_SANE_PATH_ENTRIES: &[&str] = &[
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_home_env_override_wins() {
        // Serialized by the test harness's own env guard; a plain set/remove is
        // enough here since this is the only test touching HERMES_HOME.
        std::env::set_var("HERMES_HOME", "  /tmp/custom-hermes  ");
        assert_eq!(hermes_home(), PathBuf::from("/tmp/custom-hermes"));
        std::env::remove_var("HERMES_HOME");
    }

    /// The platform default is a contract with the Python side
    /// (`hermes_constants._get_platform_default_hermes_home`), not a snapshot of
    /// a path: the two must agree or the shell looks in a different home than
    /// the backend/installer. Taken as data so it runs on any host.
    #[test]
    fn platform_default_matches_python_side() {
        let home = Path::new("/Users/me");

        assert_eq!(
            platform_default_hermes_home_for(home, None, false),
            PathBuf::from("/Users/me/.hermes")
        );
        assert_eq!(
            platform_default_hermes_home_for(home, Some("C:\\Users\\me\\AppData\\Local"), true),
            PathBuf::from("C:\\Users\\me\\AppData\\Local\\hermes")
        );
        // LOCALAPPDATA unset falls back to ~/AppData/Local, not ~/.hermes.
        assert_eq!(
            platform_default_hermes_home_for(home, None, true),
            PathBuf::from("/Users/me/AppData/Local/hermes")
        );
        // A whitespace-only LOCALAPPDATA is treated as unset, exactly like the
        // Python `.strip()` + truthiness check.
        assert_eq!(
            platform_default_hermes_home_for(home, Some("   "), true),
            PathBuf::from("/Users/me/AppData/Local/hermes")
        );
    }
}
