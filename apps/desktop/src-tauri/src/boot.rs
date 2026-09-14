//! Desktop boot progress, and the first-launch bootstrap snapshot.
//!
//! This is the shell's half of the contract `electron/main.ts` implemented with
//! its `bootProgressState` + `advanceBootProgress` pair, and it owns both
//! directions of it: the snapshot the renderer reads once at mount, and the
//! `hermes:boot-progress` event it follows afterwards.
//!
//! ## Why an absent method here is not merely incomplete
//!
//! `useGatewayBoot` reads the snapshot as
//! `desktop.getBootProgress().then(applyDesktopBootProgress)`, written for the
//! Electron shell where the method could not be missing. Delete it and that call
//! throws a *synchronous* `TypeError` from inside a `useEffect` — which React
//! hands straight to the nearest error boundary, here the root one. The window
//! renders "Something broke in the interface" and the app never mounts at all.
//!
//! So the rule this module exists to satisfy: a path the renderer calls without
//! a guard is a path this shell must answer. "Not implemented" has to be a
//! *value* (`phase: "idle"`), never an absence.
//!
//! ## Fidelity to Electron
//!
//! Phases, messages and the monotonic progress rule are ported verbatim, because
//! the renderer's boot overlay was written against them: it merges the snapshot
//! with its own `renderer.*` steps and only ever moves the bar up. Inventing a
//! phase vocabulary here would show the user strings no translation covers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// The webview subscribes under this name — `EVENT_MAP.onBootProgress` in
/// `src/desktop-bridge/tauri-bridge.ts`, which was wired before anything emitted.
pub const BOOT_PROGRESS_EVENT: &str = "hermes:boot-progress";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// `DesktopBootProgress` from `src/global.d.ts`. Field names are the contract:
/// `rename_all` below is what the renderer's type checks against.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBootProgress {
    pub error: Option<String>,
    /// Electron's E2E-only "pretend the boot failed" switch. No flag here turns
    /// it on; it stays because the renderer's type requires the field.
    pub fake_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cloud_backend_down: Option<bool>,
    pub message: String,
    pub phase: String,
    pub progress: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    pub timestamp: u64,
}

impl DesktopBootProgress {
    /// What the renderer may read before any backend work has started.
    ///
    /// Seeded to the same values `electron/main.ts` initialises
    /// `bootProgressState` with, `running: false` included: the snapshot is read
    /// before the renderer has begun its own steps, and reporting `running: true`
    /// would tell the overlay a boot is under way when nobody started one.
    pub fn idle() -> Self {
        Self {
            error: None,
            fake_mode: false,
            is_cloud_backend_down: None,
            message: "Waiting to start Hermes backend".to_string(),
            phase: "idle".to_string(),
            progress: 0,
            retryable: None,
            running: false,
            status_code: None,
            timestamp: now_ms(),
        }
    }

    /// The one transition a snapshot takes, so the state holder never has to
    /// reach into its fields.
    fn transition(&mut self, step: Step) {
        self.phase = step.phase.to_string();
        self.message = step.message;
        self.running = step.running;
        self.error = step.error;
        self.retryable = step.retryable;

        // Electron's `advanceBootProgress` clamps to 0..100 and rounds, and only
        // moves the bar backwards when the caller opted in via `allowDecrease`.
        // A failure passes no progress at all, which is how the overlay ends up
        // showing how far the boot got before it died.
        self.progress = match (step.allow_decrease, step.progress) {
            (true, Some(progress)) => progress.min(100),
            (true, None) => self.progress,
            (false, Some(progress)) => self.progress.max(progress.min(100)),
            (false, None) => self.progress,
        };

        self.timestamp = now_ms();
    }
}

/// One transition of the boot, in the vocabulary `electron/main.ts` used.
#[derive(Debug, Clone)]
pub struct Step {
    phase: &'static str,
    message: String,
    /// `None` keeps whatever the bar already shows.
    progress: Option<u8>,
    running: bool,
    /// A failure latches here; any later step clears it.
    error: Option<String>,
    retryable: Option<bool>,
    allow_decrease: bool,
}

impl Step {
    /// A step forward. Progress only climbs, so the two producers that share this
    /// state — the shell's phases and the renderer's own `renderer.*` steps —
    /// cannot make the bar jump backwards over each other.
    pub fn running(phase: &'static str, message: impl Into<String>, progress: u8) -> Self {
        Self {
            phase,
            message: message.into(),
            progress: Some(progress),
            running: true,
            error: None,
            retryable: None,
            allow_decrease: false,
        }
    }

    /// A reconnect, which legitimately rewinds the bar — Electron needed an
    /// explicit `resetBootProgressForReconnect` for exactly this.
    pub fn rewind(phase: &'static str, message: impl Into<String>, progress: u8) -> Self {
        Self {
            phase,
            message: message.into(),
            progress: Some(progress),
            running: true,
            error: None,
            retryable: None,
            allow_decrease: true,
        }
    }

    /// The terminal failure. Keeps the bar where the boot stopped.
    ///
    /// `retryable: false` is the honest answer for this shell and the one that
    /// matters: the renderer auto-retries a *transient remote* failure (a dropped
    /// SSH tunnel, a mint timeout), while a local child that fails to spawn fails
    /// the same way on every attempt — so it goes straight to the recovery
    /// surface instead of looping.
    pub fn failed(error: impl Into<String>) -> Self {
        let error = error.into();

        Self {
            phase: "backend.error",
            message: format!("Hermes backend failed to start: {error}"),
            progress: None,
            running: false,
            error: Some(error),
            retryable: Some(false),
            allow_decrease: true,
        }
    }
}

/// The single boot-progress slot.
///
/// `std::sync::Mutex` rather than the async one `AppState` uses elsewhere: every
/// operation is a handful of scalar copies with no `await` inside, so an async
/// lock would only add a panic path. Nothing here blocks on I/O while held.
pub struct BootProgress {
    inner: Mutex<DesktopBootProgress>,
}

impl BootProgress {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DesktopBootProgress::idle()),
        }
    }

    /// The snapshot behind `getBootProgress`.
    pub fn snapshot(&self) -> DesktopBootProgress {
        self.lock().clone()
    }

    /// Apply `step` and return the result, for the caller to emit.
    pub fn apply(&self, step: Step) -> DesktopBootProgress {
        let mut current = self.lock();
        current.transition(step);

        current.clone()
    }

    /// A poisoned lock means some other thread panicked while holding it. The
    /// guarded value is a handful of scalars that a panic cannot leave half
    /// written, so recovering beats propagating the panic into the boot path.
    fn lock(&self) -> MutexGuard<'_, DesktopBootProgress> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for BootProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Record `step` and push it to the webview.
///
/// Emission is best-effort by design: the renderer subscribes from an effect that
/// runs *after* its first snapshot read, so an event emitted in between has no
/// listener yet. That is safe because the next `getBootProgress` read — and every
/// subsequent phase — carries the same state.
pub fn advance(progress: &BootProgress, app: &AppHandle, step: Step) -> DesktopBootProgress {
    let snapshot = progress.apply(step);

    crate::logging::info(&format!("[boot] {} — {}", snapshot.phase, snapshot.message));
    let _ = app.emit(BOOT_PROGRESS_EVENT, snapshot.clone());

    snapshot
}

/// `DesktopBootstrapState` from `src/global.d.ts`.
///
/// The stage and log members are `serde_json::Value` rather than ported structs:
/// the runner produces them as free-form frames, and a faithful port of
/// Electron's installer event shapes would be dead code with a maintenance cost.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopBootstrapState {
    pub active: bool,
    pub manifest: Option<serde_json::Value>,
    pub stages: BTreeMap<String, serde_json::Value>,
    pub error: Option<String>,
    pub log: Vec<serde_json::Value>,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
    pub setup_choice: Option<serde_json::Value>,
    pub unsupported_platform: Option<serde_json::Value>,
}

impl DesktopBootstrapState {
    /// "No installer ran, and none is running."
    ///
    /// `active: false` with no error, no setup choice and no unsupported-platform
    /// notice is what makes `DesktopInstallOverlay`'s `shouldShow` stay false — so
    /// the overlay mounts, reads this, and renders nothing. The alternative,
    /// leaving the method absent, crashes the window instead (see the module doc).
    pub fn inactive() -> Self {
        Self {
            active: false,
            manifest: None,
            stages: BTreeMap::new(),
            error: None,
            log: Vec::new(),
            started_at: None,
            completed_at: None,
            setup_choice: None,
            unsupported_platform: None,
        }
    }
}

/// The event name the webview subscribes under — `EVENT_MAP.onBootstrapEvent` in
/// `src/desktop-bridge/tauri-bridge.ts`.
pub const BOOTSTRAP_EVENT: &str = "hermes:bootstrap-event";

/// The bounded log ring, matching Electron's `BOOTSTRAP_LOG_RING_MAX`: a long
/// install (npm + playwright) must not grow the snapshot past what the
/// renderer's `getBootstrapState()` reply can reasonably carry.
const BOOTSTRAP_LOG_RING_MAX: usize = 500;

/// The first-launch bootstrap state machine.
///
/// The runner (`bootstrap::run_bootstrap`) is side-effect-free apart from its
/// `on_event` callback; this holder is what the callback feeds, and what the
/// renderer reads back through `getBootstrapState` / `onBootstrapEvent`. It is a
/// `std::sync::Mutex` for the same reason `BootProgress` is: every mutation is a
/// handful of scalar/JSON copies with no `await` inside.
pub struct BootstrapState {
    inner: Mutex<DesktopBootstrapState>,
    /// Best-effort cancellation: the runner checks it between stages and the
    /// cancel command flips it. Not a hard kill of the in-flight install script
    /// (that would need the child's handle), but it stops the next stage from
    /// starting — which is what the overlay's Cancel button asks for.
    cancel: AtomicBool,
}

impl BootstrapState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DesktopBootstrapState::inactive()),
            cancel: AtomicBool::new(false),
        }
    }

    /// The snapshot behind `getBootstrapState`.
    pub fn snapshot(&self) -> DesktopBootstrapState {
        self.lock().clone()
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn clear_cancel(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    /// The raw flag the runner checks between stages.
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    /// Apply a runner event and return the result, for the caller to broadcast.
    pub fn apply_runner_event(&self, event: &crate::bootstrap::BootstrapEvent) -> DesktopBootstrapState {
        let mut state = self.lock();

        match event {
            crate::bootstrap::BootstrapEvent::Manifest { stages, protocol_version } => {
                state.manifest = Some(serde_json::json!({ "type": "manifest", "stages": stages, "protocolVersion": protocol_version }));
                state.active = true;
                state.setup_choice = None;
                state.started_at = Some(state.started_at.unwrap_or_else(now_ms));
                state.stages = BTreeMap::new();

                for stage in stages {
                    state.stages.insert(
                        stage.name.clone(),
                        serde_json::json!({ "state": "pending", "json": null, "durationMs": null, "error": null }),
                    );
                }
            }
            crate::bootstrap::BootstrapEvent::Stage { name, state: stage_state, duration_ms, json, error } => {
                state.stages.insert(
                    name.clone(),
                    serde_json::json!({ "state": stage_state, "durationMs": duration_ms, "json": json, "error": error }),
                );
            }
            crate::bootstrap::BootstrapEvent::Log { stage, line, stream } => {
                state.log.push(serde_json::json!({ "ts": now_ms(), "stage": stage, "line": line, "stream": stream }));

                if state.log.len() > BOOTSTRAP_LOG_RING_MAX {
                    let overflow = state.log.len() - BOOTSTRAP_LOG_RING_MAX;
                    state.log.drain(..overflow);
                }
            }
            crate::bootstrap::BootstrapEvent::Complete { .. } => {
                state.active = false;
                state.completed_at = Some(now_ms());
                state.error = None;
                state.unsupported_platform = None;
            }
            crate::bootstrap::BootstrapEvent::Failed { error, .. } => {
                state.active = false;
                state.error = Some(error.clone());
                state.setup_choice = None;
            }
        }

        state.clone()
    }

    /// Reset the snapshot to "nothing ran" (the renderer's "Reload and retry"
    /// path). Returns the fresh snapshot for the caller to broadcast.
    pub fn reset(&self) -> DesktopBootstrapState {
        let mut state = self.lock();
        *state = DesktopBootstrapState::inactive();

        state.clone()
    }

    fn lock(&self) -> MutexGuard<'_, DesktopBootstrapState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for BootstrapState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key sets below are the renderer's contract: `global.d.ts` declares
    /// these exact names, and a `rename_all` slip would break the overlay with no
    /// type error anywhere to catch it. Asserting the wire shape *is* the test of
    /// a cross-language contract — unlike a snapshot of values, which would just
    /// freeze today's numbers.
    fn keys(value: &serde_json::Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("a serialize struct is an object")
            .keys()
            .cloned()
            .collect();

        keys.sort();
        keys
    }

    #[test]
    fn the_idle_snapshot_carries_the_keys_the_renderer_declares() {
        let snapshot = serde_json::to_value(DesktopBootProgress::idle()).unwrap();

        assert_eq!(
            keys(&snapshot),
            [
                "error",
                "fakeMode",
                "message",
                "phase",
                "progress",
                "running",
                "timestamp",
            ]
        );
    }

    #[test]
    fn the_bootstrap_snapshot_carries_the_keys_the_renderer_declares() {
        let snapshot = serde_json::to_value(DesktopBootstrapState::inactive()).unwrap();

        assert_eq!(
            keys(&snapshot),
            [
                "active",
                "completedAt",
                "error",
                "log",
                "manifest",
                "setupChoice",
                "stages",
                "startedAt",
                "unsupportedPlatform",
            ]
        );
    }

    #[test]
    fn nothing_active_is_what_hides_the_installer_overlay() {
        let snapshot = serde_json::to_value(DesktopBootstrapState::inactive()).unwrap();

        // `DesktopInstallOverlay` reads these four to decide whether to render.
        assert_eq!(snapshot["active"], serde_json::json!(false));
        assert_eq!(snapshot["error"], serde_json::json!(null));
        assert_eq!(snapshot["setupChoice"], serde_json::json!(null));
        assert_eq!(snapshot["unsupportedPlatform"], serde_json::json!(null));
    }

    /// The renderer's own steps run concurrently with these, and its merge takes
    /// a `max` — so a shell phase that reports a lower number must not pull the
    /// bar down under it.
    #[test]
    fn the_bar_climbs_over_a_concurrent_step_but_a_rewind_moves_it_back() {
        let cases = [
            (
                "an out-of-order phase cannot pull it down",
                Step::running("backend.spawn", "spawning", 84),
                Step::running("backend.resolve", "resolving", 8),
                84,
            ),
            (
                "the next real phase moves it up",
                Step::running("backend.spawn", "spawning", 84),
                Step::running("backend.ready", "ready", 96),
                96,
            ),
            (
                "a reconnect rewinds it on purpose",
                Step::running("backend.spawn", "spawning", 84),
                Step::rewind("backend.resolve", "restarting", 4),
                4,
            ),
            (
                "a failure holds the bar where the boot died",
                Step::running("backend.spawn", "spawning", 84),
                Step::failed("spawn denied"),
                84,
            ),
            (
                "progress is clamped to the bar's range",
                Step::running("backend.spawn", "spawning", 84),
                Step::running("backend.ready", "ready", 250),
                100,
            ),
        ];

        for (name, first, second, expected) in cases {
            let progress = BootProgress::new();
            progress.apply(first);

            assert_eq!(progress.apply(second).progress, expected, "{name}");
        }
    }

    /// A failure with `running: true` would leave the CONNECTING overlay up over
    /// the recovery surface, and a stale `retryable: true` would make the
    /// renderer auto-retry a local spawn that fails identically every time.
    #[test]
    fn a_failure_stops_running_and_refuses_an_automatic_retry() {
        let progress = BootProgress::new();
        progress.apply(Step::running("backend.spawn", "spawning", 84));

        let failed = progress.apply(Step::failed("hermes: not found"));

        assert_eq!(failed.running, false);
        assert_eq!(failed.retryable, Some(false));
        assert_eq!(failed.phase, "backend.error");
        assert_eq!(failed.error.as_deref(), Some("hermes: not found"));
        assert!(
            failed.message.contains("hermes: not found"),
            "the overlay shows `message`, so it has to carry the reason: {}",
            failed.message
        );
    }

    /// The renderer keeps a latched error on screen until a snapshot clears it
    /// (`store/boot.ts`), so a recovery attempt has to be able to.
    #[test]
    fn a_later_step_clears_a_latched_failure() {
        let progress = BootProgress::new();
        progress.apply(Step::failed("spawn denied"));

        let recovered = progress.apply(Step::rewind("backend.resolve", "retrying", 4));

        assert_eq!(recovered.error, None);
        assert_eq!(recovered.retryable, None);
        assert_eq!(recovered.running, true);
    }

    /// A new boot must not inherit the previous one's numbers.
    #[test]
    fn the_slot_starts_idle() {
        let snapshot = BootProgress::new().snapshot();

        assert_eq!(snapshot.phase, "idle");
        assert_eq!(snapshot.progress, 0);
        assert_eq!(snapshot.running, false);
        assert_eq!(snapshot.error, None);
        assert_eq!(snapshot.fake_mode, false);
    }
}
