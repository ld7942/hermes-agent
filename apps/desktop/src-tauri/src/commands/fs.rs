//! Filesystem commands the renderer uses for plugin discovery, the workspace
//! picker, and Reveal-in-Finder affordances.
//!
//! Ports the `hermes:fs:*` handlers. The return shapes are the renderer's
//! contracts, not incidental: `readDir` returns `{ entries }` (the renderer
//! destructures it) and `readFileText` returns `{ text, truncated }`.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::logging;
use crate::paths;

/// Default cap on a single `readFileText`. Previews read whole files into the
/// renderer, so an unbounded read of a log or a large JSON would stall the tab.
const DEFAULT_READ_TEXT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntryInfo {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
}

#[derive(Debug, Serialize)]
pub struct ReadDirResult {
    pub entries: Vec<DirEntryInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadTextResult {
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct OkResult {
    pub ok: bool,
}

fn ok() -> OkResult {
    OkResult { ok: true }
}

#[tauri::command]
pub async fn hermes_fs_read_dir(dir_path: String) -> Result<ReadDirResult, String> {
    let entries = std::fs::read_dir(&dir_path).map_err(|err| format!("cannot read {dir_path}: {err}"))?;

    let mut out: Vec<DirEntryInfo> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // `file_type()` can fail on a broken symlink; fall back to metadata so a
        // single bad entry never fails the whole listing.
        let is_directory = entry
            .file_type()
            .map(|kind| kind.is_dir())
            .unwrap_or_else(|_| path.is_dir());

        out.push(DirEntryInfo {
            name,
            path: path.to_string_lossy().to_string(),
            is_directory,
        });
    }

    // A stable order keeps the renderer's plugin walk deterministic across
    // filesystems that return entries in arbitrary order.
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    Ok(ReadDirResult { entries: out })
}

#[tauri::command]
pub async fn hermes_fs_read_text(
    file_path: String,
    max_bytes: Option<usize>,
) -> Result<ReadTextResult, String> {
    let max_bytes = max_bytes.unwrap_or(DEFAULT_READ_TEXT_LIMIT);

    let bytes = std::fs::read(&file_path).map_err(|err| format!("cannot read {file_path}: {err}"))?;
    let truncated = bytes.len() > max_bytes;
    let slice = if truncated { &bytes[..max_bytes] } else { &bytes[..] };

    Ok(ReadTextResult {
        text: String::from_utf8_lossy(slice).to_string(),
        truncated,
    })
}

#[tauri::command]
pub async fn hermes_fs_write_text(file_path: String, content: String) -> Result<OkResult, String> {
    let path = PathBuf::from(&file_path);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }

    std::fs::write(&path, content).map_err(|err| format!("cannot write {file_path}: {err}"))?;

    Ok(ok())
}

/// Walk up from `start_path` looking for a `.git` entry. Returns the directory
/// containing it, or `null` when the path is outside any repository.
#[tauri::command]
pub async fn hermes_fs_git_root(start_path: String) -> Result<Option<String>, String> {
    let mut current = PathBuf::from(&start_path);

    if !current.is_dir() {
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        }
    }

    loop {
        if current.join(".git").exists() {
            return Ok(Some(current.to_string_lossy().to_string()));
        }

        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return Ok(None),
        }
    }
}

#[tauri::command]
pub async fn hermes_fs_desktop_plugins_root() -> Result<String, String> {
    Ok(paths::hermes_home()
        .join("desktop-plugins")
        .to_string_lossy()
        .to_string())
}

#[tauri::command]
pub async fn hermes_fs_logs_root() -> Result<String, String> {
    Ok(paths::logs_dir().to_string_lossy().to_string())
}

#[tauri::command]
pub async fn hermes_fs_agent_plugins_root() -> Result<String, String> {
    Ok(paths::hermes_home().join("plugins").to_string_lossy().to_string())
}

/// Reveal in the OS file manager, selecting the target when the platform can.
#[tauri::command]
pub async fn hermes_fs_reveal(target_path: String) -> Result<OkResult, String> {
    let path = PathBuf::from(&target_path);

    let spawned = if cfg!(windows) {
        // `explorer /select,<path>` needs the path unquoted and no space after
        // the comma, or Explorer opens Documents instead.
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg("-R").arg(&path).spawn()
    } else {
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent().map(Path::to_path_buf).unwrap_or(path.clone())
        };

        std::process::Command::new("xdg-open").arg(dir).spawn()
    };

    spawned.map_err(|err| format!("cannot reveal {target_path}: {err}"))?;

    Ok(ok())
}

/// Open a directory in the OS file manager (no selection).
#[tauri::command]
pub async fn hermes_fs_open_dir(dir_path: String) -> Result<OkResult, String> {
    let path = PathBuf::from(&dir_path);

    let result = if cfg!(windows) {
        std::process::Command::new("explorer").arg(&path).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(&path).spawn()
    };

    result.map_err(|err| format!("cannot open {dir_path}: {err}"))?;

    Ok(ok())
}

#[tauri::command]
pub async fn hermes_fs_rename(target_path: String, new_name: String) -> Result<OkResult, String> {
    let trimmed = new_name.trim();

    // A name with a separator would let the renderer move a file out of the
    // folder the user is looking at.
    if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(format!("invalid file name: {new_name}"));
    }

    let source = PathBuf::from(&target_path);
    let parent = source
        .parent()
        .ok_or_else(|| format!("{target_path} has no parent directory"))?;
    let destination = parent.join(trimmed);

    std::fs::rename(&source, &destination).map_err(|err| format!("cannot rename {target_path}: {err}"))?;

    Ok(ok())
}

/// Move a file or directory to the OS trash.
///
/// Deliberately not `remove_file`: the renderer's "delete" affordance is a
/// recoverable one in the Electron shell and must stay recoverable.
#[tauri::command]
pub async fn hermes_fs_trash(target_path: String) -> Result<OkResult, String> {
    let path = PathBuf::from(&target_path);

    if !path.exists() {
        // Already gone is a success — the caller wanted it gone.
        return Ok(ok());
    }

    if cfg!(windows) {
        // Recycle Bin via the VisualBasic FileIO helper, the same API
        // Explorer's own delete uses.
        let script = format!(
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{}','OnlyErrorDialogs','SendToRecycleBin')",
            target_path.replace('\'', "''")
        );

        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status()
            .map_err(|err| format!("cannot move {target_path} to the recycle bin: {err}"))?;

        if !status.success() {
            // Directory targets need the directory overload.
            let script = format!(
                "Add-Type -AssemblyName Microsoft.VisualBasic; \
                 [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory('{}','OnlyErrorDialogs','SendToRecycleBin')",
                target_path.replace('\'', "''")
            );

            let status = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .status()
                .map_err(|err| format!("cannot move {target_path} to the recycle bin: {err}"))?;

            if !status.success() {
                return Err(format!("cannot move {target_path} to the recycle bin"));
            }
        }
    } else if cfg!(target_os = "macos") {
        // No native trash CLI ships with macOS; moving into ~/.Trash is the
        // behavior Finder itself performs.
        let file_name = path
            .file_name()
            .ok_or_else(|| format!("{target_path} has no file name"))?
            .to_string_lossy()
            .to_string();
        let trash = paths::home_dir().join(".Trash");

        std::fs::create_dir_all(&trash)
            .map_err(|err| format!("cannot create {}: {err}", trash.display()))?;

        let mut destination = trash.join(&file_name);
        let mut suffix = 2;

        while destination.exists() {
            destination = trash.join(format!("{file_name} {suffix}"));
            suffix += 1;
        }

        std::fs::rename(&path, &destination).map_err(|err| format!("cannot move {target_path} to Trash: {err}"))?;
    } else {
        let status = std::process::Command::new("gio")
            .args(["trash", "--", &target_path])
            .status()
            .map_err(|err| format!("cannot move {target_path} to the trash: {err}"))?;

        if !status.success() {
            return Err(format!("cannot move {target_path} to the trash"));
        }
    }

    logging::info(&format!("trashed {target_path}"));

    Ok(ok())
}

/// Not part of the IPC surface: `writeTextFile` callers that need the
/// directory to exist go through here in tests.
#[cfg(test)]
fn ensure_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_root_walks_up_to_the_repository() {
        let temp = std::env::temp_dir().join(format!("hermes-git-root-{}", uuid::Uuid::new_v4()));
        let nested = temp.join("a").join("b");

        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(temp.join(".git")).unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let found = runtime.block_on(hermes_fs_git_root(nested.to_string_lossy().to_string()));

        assert_eq!(found.unwrap(), Some(temp.to_string_lossy().to_string()));

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn git_root_returns_none_outside_a_repository() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        // The temp root of a CI box is never inside a git checkout.
        let orphan = std::env::temp_dir().join(format!("hermes-no-git-{}", uuid::Uuid::new_v4()));

        std::fs::create_dir_all(&orphan).unwrap();

        let found = runtime.block_on(hermes_fs_git_root(orphan.to_string_lossy().to_string()));
        assert_eq!(found.unwrap(), None);

        std::fs::remove_dir_all(&orphan).ok();
    }

    #[test]
    fn read_dir_reports_directories() {
        let temp = std::env::temp_dir().join(format!("hermes-read-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(temp.join("sub")).unwrap();
        std::fs::write(temp.join("file.txt"), "hi").unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime
            .block_on(hermes_fs_read_dir(temp.to_string_lossy().to_string()))
            .unwrap();

        let names: Vec<&str> = result.entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["file.txt", "sub"]);
        assert!(result.entries[1].is_directory);
        assert!(!result.entries[0].is_directory);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn rename_rejects_a_path_separator() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(hermes_fs_rename("/tmp/x".to_string(), "../escape".to_string()));

        assert!(result.is_err());
    }

    #[test]
    fn write_text_creates_missing_parents() {
        let temp = std::env::temp_dir().join(format!("hermes-write-{}", uuid::Uuid::new_v4()));
        let target = temp.join("deep").join("nested").join("out.txt");

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(hermes_fs_write_text(
                target.to_string_lossy().to_string(),
                "content".to_string(),
            ))
            .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "content");
        assert!(ensure_parent(&target).is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }
}
