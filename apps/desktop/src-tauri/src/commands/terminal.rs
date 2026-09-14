//! PTY command surface. Thin wrappers over `crate::terminal` that own the
//! session registry lock.

use serde::Deserialize;
use tauri::{AppHandle, State, Window};

use crate::state::AppState;
use crate::terminal::{
    self, TerminalStartOptions, TerminalStartResult,
};

#[derive(Debug, Deserialize)]
pub struct TerminalResizeOptions {
    #[serde(default)]
    pub cols: Option<u32>,
    #[serde(default)]
    pub rows: Option<u32>,
}

#[tauri::command]
pub async fn hermes_terminal_start(
    app: AppHandle,
    window: Window,
    state: State<'_, AppState>,
    options: Option<TerminalStartOptions>,
) -> Result<TerminalStartResult, String> {
    let options = options.unwrap_or(TerminalStartOptions {
        cwd: None,
        cols: None,
        rows: None,
    });

    let (result, session) = terminal::start_terminal(&app, window.label(), options)?;

    state.terminals.lock().await.insert(result.id.clone(), session);

    Ok(result)
}

#[tauri::command]
pub async fn hermes_terminal_attach(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    Ok(state.terminals.lock().await.contains_key(&id))
}

#[tauri::command]
pub async fn hermes_terminal_write(
    state: State<'_, AppState>,
    id: String,
    data: String,
) -> Result<bool, String> {
    let mut terminals = state.terminals.lock().await;

    match terminals.get_mut(&id) {
        Some(session) => Ok(terminal::write_terminal(session, &data)),
        None => Ok(false),
    }
}

#[tauri::command]
pub async fn hermes_terminal_resize(
    state: State<'_, AppState>,
    id: String,
    size: Option<TerminalResizeOptions>,
) -> Result<bool, String> {
    let size = size.unwrap_or(TerminalResizeOptions {
        cols: None,
        rows: None,
    });

    let mut terminals = state.terminals.lock().await;

    match terminals.get_mut(&id) {
        Some(session) => Ok(terminal::resize_terminal(session, size.cols, size.rows)),
        None => Ok(false),
    }
}

/// The shell's working directory. The Electron implementation resolved this
/// through the OS (`/proc/<pid>/cwd`, `lsof`); the Tauri shell reports the
/// directory the session was started in, which is what the tab-restore path
/// needs and never fails on a platform without those tools.
#[tauri::command]
pub async fn hermes_terminal_cwd(state: State<'_, AppState>, id: String) -> Result<Option<String>, String> {
    let terminals = state.terminals.lock().await;

    Ok(terminals.get(&id).map(|session| session.cwd.clone()))
}

#[tauri::command]
pub async fn hermes_terminal_dispose(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let mut terminals = state.terminals.lock().await;

    match terminals.remove(&id) {
        Some(mut session) => {
            terminal::dispose_terminal(&mut session);

            Ok(true)
        }
        None => Ok(false),
    }
}

/// The shell the next terminal would spawn, for the Settings preview row.
#[tauri::command]
pub async fn hermes_terminal_shell() -> Result<String, String> {
    Ok(terminal::resolve_shell_name())
}
