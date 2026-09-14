//! Tauri command surface.
//!
//! One module per IPC namespace, mirroring how the Electron main process
//! grouped its `ipcMain.handle` registrations. Command names are the Electron
//! channel with `:` flattened to `_` (`hermes:connection` → `hermes_connection`);
//! the renderer-side bridge performs that same mapping, so the renderer keeps
//! calling `window.hermesDesktop.getConnection(...)` unchanged.

pub mod backend;
pub mod boot;
pub mod fs;
pub mod system;
pub mod terminal;
pub mod window;
