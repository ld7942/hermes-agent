//! The boot-progress read, and the first-launch bootstrap commands.
//!
//! `hermes_boot_progress_get` / `hermes_bootstrap_state_get` exist as commands
//! rather than as entries in the bridge's "not implemented" list because the
//! renderer calls them on the boot path without a guard — see `crate::boot` for
//! what that costs when they are absent. The four mutating commands
//! (`start` / `cancel` / `reset` / `repair`) back the renderer's install
//! overlay buttons.

use tauri::{AppHandle, Emitter, Manager, State};

use crate::boot::{self, DesktopBootProgress, DesktopBootstrapState};
use crate::bootstrap::{self, BootstrapEvent, BootstrapOutcome};
use crate::paths;
use crate::state::AppState;

/// The snapshot `useGatewayBoot` reads once at mount, before it subscribes to
/// `hermes:boot-progress`. Answered from state rather than recomputed, so a
/// renderer that reads late sees the phase the boot actually reached.
#[tauri::command]
pub async fn hermes_boot_progress_get(state: State<'_, AppState>) -> Result<DesktopBootProgress, String> {
    Ok(state.boot.snapshot())
}

/// The first-launch installer snapshot the overlay reads on mount.
#[tauri::command]
pub async fn hermes_bootstrap_state_get(state: State<'_, AppState>) -> Result<DesktopBootstrapState, String> {
    Ok(state.bootstrap.snapshot())
}

/// The `DesktopInstallOverlay` result shape: `{ ok }` for the actions whose
/// call sites only check success, `{ ok, cancelled }` for Cancel.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapActionResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

/// Drive the first-launch install (`continueBootstrapLocal`). Fire-and-forget
/// from the renderer's perspective, exactly like Electron's `continue-local`:
/// it returns `{ ok: true }` immediately and the install runs behind the event
/// stream the overlay is already subscribed to.
#[tauri::command]
pub async fn hermes_bootstrap_start(state: State<'_, AppState>, app: AppHandle) -> Result<BootstrapActionResult, String> {
    run_bootstrap_flow(&state.http, &state.bootstrap, &app, false).await;

    Ok(BootstrapActionResult { ok: true, cancelled: None })
}

/// Cancel the in-flight install. Returns `{ ok: false, cancelled: false }` when
/// nothing is running, mirroring Electron's abort-controller-shaped reply.
#[tauri::command]
pub async fn hermes_bootstrap_cancel(state: State<'_, AppState>) -> Result<BootstrapActionResult, String> {
    let was_active = state.bootstrap.snapshot().active;

    if !was_active {
        return Ok(BootstrapActionResult { ok: false, cancelled: Some(false) });
    }

    state.bootstrap.request_cancel();

    Ok(BootstrapActionResult { ok: true, cancelled: Some(true) })
}

/// Clear a latched failure and the snapshot so the next boot re-drives the flow
/// (`resetBootstrap` / "Reload and retry").
#[tauri::command]
pub async fn hermes_bootstrap_reset(state: State<'_, AppState>, app: AppHandle) -> Result<BootstrapActionResult, String> {
    state.bootstrap.reset();
    let _ = app.emit(boot::BOOTSTRAP_EVENT, serde_json::json!({ "type": "dismissed" }));

    Ok(BootstrapActionResult { ok: true, cancelled: None })
}

/// Forceful repair: re-run the installer (stages are idempotent) and clear any
/// latched failure. The Electron shell distinguishes soft restart from hard
/// reinstall via `decideBootstrapRepair`; this shell always re-runs the
/// installer, which is the safe superset.
#[tauri::command]
pub async fn hermes_bootstrap_repair(state: State<'_, AppState>, app: AppHandle) -> Result<BootstrapActionResult, String> {
    run_bootstrap_flow(&state.http, &state.bootstrap, &app, true).await;

    Ok(BootstrapActionResult { ok: true, cancelled: None })
}

/// The shared start/repair/ensure path: resolve the stamp and roots, then run
/// the installer while feeding every event into the state machine and the
/// webview. Takes the disjoint pieces rather than `&AppState` so `ensure_backend`
/// can call it while still holding the backend lock.
pub(crate) async fn run_bootstrap_flow(
    http: &reqwest::Client,
    bootstrap_state: &boot::BootstrapState,
    app: &AppHandle,
    _repair: bool,
) -> BootstrapOutcome {
    let resource_dir = app.path().resource_dir().ok();
    let stamp = bootstrap::resolve_install_stamp(resource_dir.as_deref());
    let hermes_home = paths::hermes_home();
    let active_root = bootstrap::active_root(&hermes_home);
    let is_windows = cfg!(windows);

    bootstrap_state.clear_cancel();

    // Eagerly flip the UI to "active" with an empty manifest so the overlay
    // shows before the real manifest returns (a cold network fetch can take
    // tens of seconds). The real manifest event overwrites it.
    let eager = BootstrapEvent::Manifest {
        stages: Vec::new(),
        protocol_version: None,
    };
    bootstrap_state.apply_runner_event(&eager);
    let _ = app.emit(boot::BOOTSTRAP_EVENT, &eager);

    let cancel = bootstrap_state.cancel_flag();

    let mut on_event = |event: BootstrapEvent| {
        bootstrap_state.apply_runner_event(&event);
        let _ = app.emit(boot::BOOTSTRAP_EVENT, &event);
    };

    bootstrap::run_bootstrap(http, stamp.as_ref(), &active_root, None, &hermes_home, is_windows, cancel, &mut on_event).await
}
