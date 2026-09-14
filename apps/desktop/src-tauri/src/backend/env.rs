//! Environment construction for the spawned backend.
//!
//! Port of `electron/backend-env.ts`. The spawned Python backend needs three
//! things the Finder/Dock-launched shell does not inherit:
//!   * Hermes-managed Node directories on PATH (plugins shell out to node);
//!   * the managed venv's bin directory (so `hermes`, `python`, `pip` resolve);
//!   * POSIX sane PATH entries, because macOS GUI apps start with a PATH that
//!     misses Homebrew entirely.
//!
//! PYTHONUTF8 is forced so stdio defaults to UTF-8 even on a GBK/cp1252
//! Windows locale — anything the interpreter emits before the Python bootstrap
//! runs would otherwise decode with the locale default.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::paths;

fn path_delimiter() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

/// Append entries in priority order, dropping duplicates and empties.
fn append_unique(entries: Vec<PathBuf>) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered: Vec<String> = Vec::new();

    for entry in entries {
        let text = entry.to_string_lossy().to_string();

        if text.trim().is_empty() {
            continue;
        }

        // A delimiter-containing entry is split so a PATH variable hoisted into
        // this list keeps its individual entries deduped.
        for part in text.split(path_delimiter()) {
            if part.is_empty() || !seen.insert(part.to_string()) {
                continue;
            }

            ordered.push(part.to_string());
        }
    }

    ordered.join(&path_delimiter().to_string())
}

/// The PATH the backend is spawned with.
pub fn desktop_backend_path(hermes_home: &Path, venv_root: &Path) -> String {
    let mut entries: Vec<PathBuf> = Vec::new();

    entries.extend(paths::hermes_node_path_entries());

    // Recompute against the venv actually in use (HERMES_VENV may differ from
    // <hermes_home>/venv).
    let _ = hermes_home;
    let venv_bin = if cfg!(windows) {
        venv_root.join("Scripts")
    } else {
        venv_root.join("bin")
    };
    entries.push(venv_bin);

    if let Some(current) = std::env::var_os("PATH") {
        entries.push(PathBuf::from(current));
    }

    if !cfg!(windows) {
        entries.extend(paths::POSIX_SANE_PATH_ENTRIES.iter().map(PathBuf::from));
    }

    append_unique(entries)
}

/// Environment deltas layered onto the inherited environment for the backend.
pub fn build_backend_env(hermes_home: &Path, venv_root: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();

    let current_python_path = std::env::var("PYTHONPATH").unwrap_or_default();
    let python_path = append_unique(
        [current_python_path]
            .iter()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .collect(),
    );

    env.push(("PYTHONPATH".to_string(), python_path));

    // A user's explicit setting wins; otherwise force UTF-8 mode.
    let python_utf8 = std::env::var("PYTHONUTF8").unwrap_or_else(|_| "1".to_string());
    env.push(("PYTHONUTF8".to_string(), python_utf8));

    env.push((
        "PATH".to_string(),
        desktop_backend_path(hermes_home, venv_root),
    ));

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venv_bin_precedes_inherited_path() {
        let delimiter = path_delimiter();
        let original = std::env::var_os("PATH");

        // Built with this platform's delimiter, so the split below sees exactly
        // two entries — a literal `:` is one entry on Windows and two on POSIX.
        std::env::set_var("PATH", ["/usr", "/bin"].join(&delimiter.to_string()));

        let path = desktop_backend_path(Path::new("/tmp/hermes"), Path::new("/tmp/hermes/venv"));
        let parts: Vec<&str> = path.split(delimiter).collect();

        let venv_dir = if cfg!(windows) { "Scripts" } else { "bin" };

        let venv_index = parts
            .iter()
            .position(|part| part.ends_with(venv_dir))
            .expect("venv bin present");
        let inherited_index = parts
            .iter()
            .position(|part| *part == "/usr")
            .expect("inherited PATH preserved");

        assert!(venv_index < inherited_index, "venv bin must win: {parts:?}");

        match original {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }

    #[test]
    fn duplicates_are_dropped() {
        let delimiter = path_delimiter();
        let unit = delimiter.to_string();

        // Each entry is a whole PATH value that must be split before deduping,
        // which is how `desktop_backend_path` sees the inherited PATH.
        let joined = append_unique(vec![
            PathBuf::from(format!("/a{unit}/b")),
            PathBuf::from(format!("/b{unit}/c")),
        ]);
        let parts: Vec<&str> = joined.split(delimiter).collect();

        assert_eq!(parts, vec!["/a", "/b", "/c"]);
    }
}
