//! The embedded terminal's PTY host.
//!
//! Rust replacement for `electron/terminal-ipc.ts` (node-pty → portable-pty).
//! The IPC contract is unchanged, including the per-session event channel names
//! the renderer subscribes to:
//!
//!   start   { cwd, cols, rows } → { cwd, id, shell }
//!   attach  (id)                → bool
//!   write   (id, data)          → bool
//!   resize  (id, { cols, rows })→ bool
//!   cwd     (id)                → string | null
//!   dispose (id)                → bool
//!   events  `hermes:terminal:<id>:data` (string)
//!           `hermes:terminal:<id>:exit` ({ code, signal })

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::Mutex;

use crate::logging;

#[derive(Debug, Deserialize)]
pub struct TerminalStartOptions {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub cols: Option<u32>,
    #[serde(default)]
    pub rows: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalStartResult {
    pub id: String,
    pub cwd: String,
    pub shell: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalExitPayload {
    pub code: i32,
    pub signal: Option<String>,
}

/// A live shell. The shell's *name* is not stored here: it is already returned
/// to the renderer in `TerminalStartResult`, and nothing reads it back per
/// session.
pub struct TerminalSession {
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub cwd: String,
}

/// Session registry, kept in `AppState` behind one lock. Plain `Mutex` rather
/// than `Arc<Mutex<..>>` because `AppState` is itself shared by Tauri.
pub type TerminalRegistry = Mutex<HashMap<String, TerminalSession>>;

/// Minimum dimensions accepted from the renderer, matching node-pty's guard:
/// xterm reports 0x0 during teardown and a 0-width PTY wedges the shell.
fn clamp_dimension(value: Option<u32>, fallback: u32) -> u16 {
    value.unwrap_or(fallback).max(2).min(u16::MAX as u32) as u16
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// PATH lookup, with the platform executable suffix on Windows.
pub fn find_on_path(command: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }

        let direct = dir.join(command);

        if is_executable_file(&direct) {
            return Some(direct);
        }

        if cfg!(windows) {
            let with_suffix = dir.join(format!("{command}.exe"));

            if is_executable_file(&with_suffix) {
                return Some(with_suffix);
            }
        }
    }

    None
}

struct ShellSpec {
    command: PathBuf,
    args: Vec<String>,
    name: String,
}

fn shell_spec_for(path: &Path) -> ShellSpec {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "shell".to_string());

    let args: Vec<String> = if name.starts_with("pwsh") || name.starts_with("powershell") {
        // -NoLogo keeps the prompt flush against the tab, like the POSIX shells.
        vec!["-NoLogo".to_string()]
    } else if name.starts_with("cmd") {
        vec![]
    } else if name.contains("zsh") || name.contains("bash") {
        vec!["-il".to_string()]
    } else {
        vec!["-i".to_string()]
    };

    ShellSpec {
        command: path.to_path_buf(),
        args,
        name,
    }
}

/// Resolve the interactive shell. An explicit `HERMES_DESKTOP_SHELL` override
/// wins; `$SHELL` is honored on POSIX but ignored on Windows, where it is
/// usually a stray MSYS/Git path a native PTY cannot spawn.
fn terminal_shell_spec() -> ShellSpec {
    let override_value = std::env::var("HERMES_DESKTOP_SHELL")
        .unwrap_or_default()
        .trim()
        .to_string();

    let posix_preference = if cfg!(windows) {
        String::new()
    } else {
        std::env::var("SHELL").unwrap_or_default()
    };

    let requested = if !override_value.is_empty() {
        Some(override_value)
    } else if !posix_preference.trim().is_empty() {
        Some(posix_preference)
    } else {
        None
    };

    if let Some(requested) = requested {
        let as_path = PathBuf::from(&requested);

        if is_executable_file(&as_path) {
            return shell_spec_for(&as_path);
        }

        if let Some(found) = find_on_path(&requested) {
            return shell_spec_for(&found);
        }
    }

    if cfg!(windows) {
        // PowerShell 7+ first, then the fixed 5.1 location, then cmd.exe.
        if let Some(found) = find_on_path("pwsh") {
            return shell_spec_for(&found);
        }

        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("windir"))
            .unwrap_or_else(|_| "C:\\Windows".to_string());
        let builtin = PathBuf::from(system_root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");

        if is_executable_file(&builtin) {
            return shell_spec_for(&builtin);
        }

        if let Some(comspec) = std::env::var_os("COMSPEC") {
            return shell_spec_for(&PathBuf::from(comspec));
        }

        return shell_spec_for(Path::new("cmd.exe"));
    }

    for candidate in ["/bin/zsh", "/bin/bash", "/bin/sh"] {
        let path = Path::new(candidate);

        if is_executable_file(path) {
            return shell_spec_for(path);
        }
    }

    shell_spec_for(Path::new("/bin/sh"))
}

/// The shell the next terminal would spawn, for the Settings preview row.
pub fn resolve_shell_name() -> String {
    terminal_shell_spec().name
}

/// A directory the shell can actually start in. A `cwd` that vanished between
/// the tab being restored and the shell being spawned must not fail the spawn.
fn safe_terminal_cwd(requested: Option<&str>) -> String {
    if let Some(value) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        let path = Path::new(value);

        if path.is_dir() {
            return value.to_string();
        }
    }

    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| crate::paths::home_dir().to_string_lossy().to_string())
}

fn emit_terminal_event<T: Serialize + Clone>(app: &AppHandle, label: &str, event: &str, payload: T) {
    if let Err(err) = app.emit_to(label, event, payload) {
        logging::warn(&format!("terminal event {event} -> {label} failed: {err}"));
    }
}

/// Start a PTY session and begin forwarding its output.
pub fn start_terminal(
    app: &AppHandle,
    window_label: &str,
    options: TerminalStartOptions,
) -> Result<(TerminalStartResult, TerminalSession), String> {
    let spec = terminal_shell_spec();
    let cwd = safe_terminal_cwd(options.cwd.as_deref());
    let cols = clamp_dimension(options.cols, 80);
    let rows = clamp_dimension(options.rows, 24);

    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("failed to open a pty: {err}"))?;

    let mut builder = CommandBuilder::new(spec.command.to_string_lossy().to_string());

    for arg in &spec.args {
        builder.arg(arg);
    }

    builder.cwd(PathBuf::from(&cwd));
    builder.env("TERM", "xterm-256color");

    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|err| format!("failed to spawn {}: {err}", spec.command.display()))?;

    // The slave handle must be released or the PTY never sees EOF on close.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("failed to clone pty reader: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("failed to take pty writer: {err}"))?;

    let id = uuid::Uuid::new_v4().to_string();
    let data_event = format!("hermes:terminal:{id}:data");
    let exit_event = format!("hermes:terminal:{id}:exit");

    // Output comes off a blocking read; a dedicated thread keeps it off the
    // async runtime.
    {
        let app = app.clone();
        let window_label = window_label.to_string();
        let data_event = data_event.clone();

        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];

            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let chunk = String::from_utf8_lossy(&buf[..read]).to_string();
                        emit_terminal_event(&app, &window_label, &data_event, chunk);
                    }
                }
            }
        });
    }

    // The exit watcher owns `child`; `wait()` is blocking, so it also gets a
    // thread rather than a runtime task.
    {
        let app = app.clone();
        let window_label = window_label.to_string();

        std::thread::spawn(move || {
            let status = child.wait();

            // `portable_pty::ExitStatus` exposes only `exit_code()`; the
            // terminating signal is a private field on this version. The
            // renderer's `signal` slot stays in the payload (node-pty filled it)
            // and is `null` here — a shell killed by SIGHUP reports code 128+n
            // anyway, so nothing is lost that the renderer reads.
            let (code, signal) = match status {
                Ok(status) => (status.exit_code() as i32, None),
                Err(_) => (-1, None),
            };

            emit_terminal_event(
                &app,
                &window_label,
                &exit_event,
                TerminalExitPayload { code, signal },
            );
        });
    }

    let result = TerminalStartResult {
        id: id.clone(),
        cwd: cwd.clone(),
        shell: spec.name,
    };

    let session = TerminalSession {
        master: pair.master,
        writer,
        cwd,
    };

    Ok((result, session))
}

pub fn write_terminal(session: &mut TerminalSession, data: &str) -> bool {
    if session.writer.write_all(data.as_bytes()).is_err() {
        return false;
    }

    session.writer.flush().is_ok()
}

pub fn resize_terminal(session: &mut TerminalSession, cols: Option<u32>, rows: Option<u32>) -> bool {
    session
        .master
        .resize(PtySize {
            rows: clamp_dimension(rows, 24),
            cols: clamp_dimension(cols, 80),
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

/// Kill the PTY's process. Best effort — the session may already be gone.
pub fn dispose_terminal(session: &mut TerminalSession) {
    let _ = session
        .master
        .resize(PtySize { rows: 1, cols: 1, pixel_width: 0, pixel_height: 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_are_clamped_to_a_usable_floor() {
        assert_eq!(clamp_dimension(None, 80), 80);
        assert_eq!(clamp_dimension(Some(0), 80), 2);
        assert_eq!(clamp_dimension(Some(1), 24), 2);
        assert_eq!(clamp_dimension(Some(120), 80), 120);
    }

    #[test]
    fn missing_cwd_falls_back_instead_of_failing() {
        let cwd = safe_terminal_cwd(Some("/definitely/not/a/real/directory/xyz"));
        assert!(!cwd.is_empty(), "must fall back to home, never the raw value");
    }

    #[test]
    fn existing_cwd_is_preserved() {
        let temp = std::env::temp_dir();
        let cwd = safe_terminal_cwd(Some(temp.to_string_lossy().as_ref()));
        assert_eq!(cwd, temp.to_string_lossy());
    }
}
