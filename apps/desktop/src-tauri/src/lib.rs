//! Hermes desktop shell — Tauri entry point.
//!
//! Replaces the Electron main process. The renderer is unchanged: this crate
//! provides the same `window.hermesDesktop` contract over Tauri's `invoke`
//! surface, with the channel name flattened (`hermes:connection` →
//! `hermes_connection`). The mapping lives in one place on the renderer side
//! (`src/desktop-bridge/tauri-bridge.ts`), so no component knows which shell it
//! runs under.

mod backend;
mod boot;
mod bootstrap;
mod commands;
mod logging;
mod paths;
mod state;
mod terminal;

use tauri::{Manager, RunEvent};

pub fn run() {
    logging::info(&format!(
        "desktop shell starting (tauri {}) — hermes home {}",
        tauri::VERSION,
        paths::hermes_home().display()
    ));

    // Before the builder, because WebView2 reads its extra switches when it
    // creates the environment — the same reason Electron had to decide this
    // ahead of `app.whenReady()`.
    commands::system::apply_remote_display_fallback();

    tauri::Builder::default()
        // Plugins replace the Electron main-process capabilities one for one:
        // dialog → file pickers, opener → shell.openExternal/shell.openPath,
        // process → app.relaunch, notification → native Notification,
        // clipboard-manager → clipboard.readText/writeText,
        // global-shortcut → globalShortcut, os → process.platform.
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_os::init())
        // A second launch (deep link, double-click, `hermes://` URL) must focus
        // the existing window instead of spawning a second backend. The main
        // window may still be hidden (declared `visible: false` and revealed by
        // the renderer's `showMainWindow` handshake); showing it here is safe —
        // an already-visible window just stays visible, and a not-yet-revealed
        // one is revealed to focus the incoming intent rather than left blank.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .setup(|app| {
            let state = state::AppState::new().map_err(|err| {
                logging::error(&err);

                err
            })?;

            app.manage(state);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // boot progress + the (inert) bootstrap snapshot
            commands::boot::hermes_boot_progress_get,
            commands::boot::hermes_bootstrap_state_get,
            commands::boot::hermes_bootstrap_start,
            commands::boot::hermes_bootstrap_cancel,
            commands::boot::hermes_bootstrap_reset,
            commands::boot::hermes_bootstrap_repair,
            // backend + connection + proxy
            commands::backend::hermes_connection,
            commands::backend::hermes_connection_for,
            commands::backend::hermes_connection_revalidate,
            commands::backend::hermes_gateway_ws_url,
            commands::backend::hermes_gateway_ws_url_for,
            commands::backend::hermes_backend_touch,
            commands::backend::hermes_backend_recycle,
            commands::backend::hermes_api,
            commands::backend::hermes_launch_flags,
            commands::backend::hermes_pool_limits_get,
            // windows
            commands::window::hermes_window_ready,
            commands::window::hermes_window_open_instance,
            commands::window::hermes_window_open_session,
            commands::window::hermes_window_open_browser,
            commands::window::hermes_window_state,
            // terminal
            commands::terminal::hermes_terminal_start,
            commands::terminal::hermes_terminal_attach,
            commands::terminal::hermes_terminal_write,
            commands::terminal::hermes_terminal_resize,
            commands::terminal::hermes_terminal_cwd,
            commands::terminal::hermes_terminal_dispose,
            commands::terminal::hermes_terminal_shell,
            // filesystem
            commands::fs::hermes_fs_read_dir,
            commands::fs::hermes_fs_read_text,
            commands::fs::hermes_fs_write_text,
            commands::fs::hermes_fs_git_root,
            commands::fs::hermes_fs_desktop_plugins_root,
            commands::fs::hermes_fs_logs_root,
            commands::fs::hermes_fs_agent_plugins_root,
            commands::fs::hermes_fs_reveal,
            commands::fs::hermes_fs_open_dir,
            commands::fs::hermes_fs_rename,
            commands::fs::hermes_fs_trash,
            // system
            commands::system::hermes_version,
            commands::system::hermes_app_relaunch,
            commands::system::hermes_open_external,
            commands::system::hermes_clipboard_read,
            commands::system::hermes_clipboard_write,
            commands::system::hermes_notify,
            commands::system::hermes_logs_reveal,
            commands::system::hermes_logs_recent,
            commands::system::hermes_logs_renderer_error,
            commands::system::hermes_setting_default_project_dir_get,
            commands::system::hermes_setting_default_project_dir_set,
            commands::system::hermes_setting_default_project_dir_pick,
            commands::system::hermes_select_paths,
            commands::system::hermes_select_save_path,
            // native chrome + power
            commands::system::hermes_native_theme_set,
            commands::system::hermes_keep_awake_set,
            commands::system::hermes_power_battery_get,
            commands::system::hermes_get_remote_display_reason,
        ])
        .build(tauri::generate_context!())
        .expect("error while building the Hermes desktop shell")
        .run(|app_handle, event| {
            if let RunEvent::Exit = event {
                teardown_backend(app_handle);
            }
        });
}

/// Kill the spawned backend when the shell exits.
///
/// The backend is a separate OS process tree (the venv shim + the gateway it
/// spawned). Without an explicit kill it outlives the shell, and every
/// close-and-relaunch piles up one more orphan holding the checkout's files
/// locked and contending on the shared home — the Electron shell tears its
/// backend down on `before-quit`; this is the Tauri equivalent.
///
/// Runs on the main thread from the `RunEvent::Exit` callback, so it blocks on
/// the async lock via Tauri's global runtime rather than the event-loop one
/// (which is already winding down here).
fn teardown_backend(app_handle: &tauri::AppHandle) {
    let state = app_handle.state::<crate::state::AppState>();
    let mut slot = tauri::async_runtime::block_on(state.backend.lock());

    if let Some(mut handle) = slot.take() {
        tauri::async_runtime::block_on(crate::backend::stop_backend(&mut handle));
    }
}
