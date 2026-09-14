//! Backend connection, the HTTP proxy, and the gateway WebSocket URL.
//!
//! Replaces the `hermes:connection`, `hermes:gateway:ws-url`, `hermes:api`,
//! `hermes:backend:touch`, and `hermes:backend:recycle` handlers.
//!
//! Scope: the local (managed-child) path only. The registry paths — remote
//! hosts, SSH tunnels, saved cloud connections — are not wired yet; they return
//! an explicit error rather than silently dialing the wrong backend. Porting
//! them means bringing `connection-config.ts` (34KB) and `connection-registry.ts`
//! over, which is the largest remaining chunk of the migration.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

use crate::backend::{self, ConnectionDescriptor};
use crate::boot::{self, Step};
use crate::logging;
use crate::state::AppState;

/// The renderer's `hermes:api` request envelope. Field names match
/// `src/api/client.ts` exactly; `path` is backend-relative (`/api/...`).
///
/// `connectionId` is deliberately absent: this shell manages only the primary
/// local backend, so every request resolves to it. serde drops unknown keys, so
/// a renderer that still sends one keeps working — and when remote connections
/// land, the field returns here together with the routing that reads it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiRequest {
    pub path: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// `GatewayWsUrlResult` from `src/global.d.ts`: a tagged union, not a throw, so
/// the renderer can distinguish "re-auth needed" from "transport down".
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayWsUrlResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_oauth_login: Option<bool>,
}

impl GatewayWsUrlResult {
    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            ws_url: None,
            error: Some(message.into()),
            needs_oauth_login: None,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OkResult {
    pub ok: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchFlags {
    pub local_models: bool,
}

/// Ensure a live backend for `profile`, spawning or recycling as needed.
///
/// Holds the backend lock across the spawn so two concurrent renderer dials
/// cannot both start a child (the Electron shell needed an explicit
/// `ensureBackend` claim queue for the same reason).
///
/// Profiles are compared through `backend::serves_profile`, never by `Option`
/// identity: an absent profile and the primary profile's name are the same
/// backend, and the boot path sends both — `getConnection()` for the
/// window-owned dial, `getConnection("default")` for routed requests. Comparing
/// the raw options made each alternating call tear down the healthy child the
/// previous one had just started.
///
/// `app` carries the webview handle for the boot-progress events. The phases are
/// reported from *below* the reuse check on purpose: `hermes_api` calls this once
/// per HTTP request, and a progress update there would overwrite `phase` and
/// `running` on every call — flipping a finished boot back to "resolving" while
/// the user is reading the app.
pub async fn ensure_backend(
    state: &AppState,
    app: &AppHandle,
    profile: Option<&str>,
) -> Result<(), String> {
    let mut slot = state.backend.lock().await;

    let reusable = match slot.as_mut() {
        Some(handle) => backend::serves_profile(handle.profile.as_deref(), profile) && backend::is_alive(handle),
        None => false,
    };

    if reusable {
        return Ok(());
    }

    // Phase names, messages and progress values are Electron's, verbatim — the
    // renderer's overlay merges them with its own steps and was written against
    // these exact strings.
    boot::advance(
        &state.boot,
        app,
        Step::running("backend.resolve", "Resolving Hermes backend", 8),
    );

    if let Some(mut stale) = slot.take() {
        // A recycle costs a full cold start, so it must be diagnosable from the
        // log: the two reasons (different profile vs dead child) have opposite
        // remedies, and neither is visible in the spawn line that follows.
        logging::info(&format!(
            "recycling backend pid {} (held profile {:?}, requested {:?})",
            stale.pid, stale.profile, profile
        ));

        backend::stop_backend(&mut stale).await;
    }

    boot::advance(
        &state.boot,
        app,
        Step::running("backend.spawn", "Starting Hermes backend", 84),
    );

    let handle = match backend::spawn_backend(profile).await {
        Ok(handle) => handle,
        // Every failure path below reports before returning: the snapshot is what
        // the renderer's recovery surface reads, and a rejection that left the
        // progress mid-spawn would pin the CONNECTING overlay over it.
        Err(err) => {
            // The one recoverable failure: no backend installed. Run the
            // first-launch installer, then retry — the Tauri equivalent of
            // Electron's `ensureRuntime` "bootstrap-needed" path.
            if backend::command::is_backend_not_found(&err) {
                logging::info("no Hermes install found; starting first-launch bootstrap");

                let outcome = crate::commands::boot::run_bootstrap_flow(
                    &state.http,
                    &state.bootstrap,
                    app,
                    false,
                )
                .await;

                if !outcome.ok {
                    let message = outcome.error.unwrap_or_else(|| "bootstrap failed".to_string());
                    boot::advance(&state.boot, app, Step::failed(&message));

                    return Err(message);
                }

                // Re-resolve now that the install exists.
                match backend::spawn_backend(profile).await {
                    Ok(handle) => handle,
                    Err(retry_err) => {
                        boot::advance(&state.boot, app, Step::failed(&retry_err));

                        return Err(retry_err);
                    }
                }
            } else {
                boot::advance(&state.boot, app, Step::failed(&err));

                return Err(err);
            }
        }
    };

    if let Err(err) = backend::wait_for_ready(&state.http, &handle.base_url, &handle.token).await {
        let mut failed = handle;
        backend::stop_backend(&mut failed).await;

        boot::advance(&state.boot, app, Step::failed(&err));

        return Err(err);
    }

    *slot = Some(handle);

    boot::advance(
        &state.boot,
        app,
        Step::running(
            "backend.ready",
            "Hermes backend is ready. Finalizing desktop startup",
            94,
        ),
    );

    Ok(())
}

#[tauri::command]
pub async fn hermes_connection(
    state: State<'_, AppState>,
    app: AppHandle,
    profile: Option<String>,
    // Accepted for call-site compatibility with the Electron preload
    // (`getConnection(profile, opts)`). The only option it carries selects a
    // remote backend, which this shell does not have — so it is not read.
    _opts: Option<serde_json::Value>,
) -> Result<ConnectionDescriptor, String> {
    ensure_backend(&state, &app, profile.as_deref()).await?;

    let slot = state.backend.lock().await;
    let handle = slot
        .as_ref()
        .ok_or_else(|| "backend unavailable after a successful start".to_string())?;

    Ok(backend::descriptor_for(handle))
}

/// Registry-scoped resolution. Only the local (empty `connectionId`) case is
/// wired; a non-empty id falls back to the primary backend rather than
/// silently dialing the wrong host.
#[tauri::command]
pub async fn hermes_connection_for(
    state: State<'_, AppState>,
    app: AppHandle,
    payload: ConnectionOptionsFor,
) -> Result<ConnectionDescriptor, String> {
    if payload
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .is_some()
    {
        return Err(
            "Registry-scoped connections (remote/SSH/cloud) are not wired in the Tauri shell yet."
                .to_string(),
        );
    }

    hermes_connection(state, app, payload.profile, None).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionOptionsFor {
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

/// Liveness re-probe. Local children self-heal through the stdout pump, so the
/// honest answer is "nothing was rebuilt" — same semantics as the Electron
/// no-op for local backends.
#[tauri::command]
pub async fn hermes_connection_revalidate(state: State<'_, AppState>) -> Result<RevalidateResult, String> {
    let mut slot = state.backend.lock().await;

    // `is_alive` needs `&mut` (it reaps via `try_wait`), which a `matches!`
    // pattern guard cannot supply — and "no handle at all" is not a dead
    // handle: there was nothing to lose, so there is nothing to rebuild.
    let dead = match slot.as_mut() {
        Some(handle) => !backend::is_alive(handle),
        None => false,
    };

    if !dead {
        return Ok(RevalidateResult {
            ok: true,
            rebuilt: false,
        });
    }

    if let Some(mut stale) = slot.take() {
        backend::stop_backend(&mut stale).await;
    }

    Ok(RevalidateResult {
        ok: true,
        rebuilt: true,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevalidateResult {
    pub ok: bool,
    pub rebuilt: bool,
}

#[tauri::command]
pub async fn hermes_gateway_ws_url(
    state: State<'_, AppState>,
    app: AppHandle,
    profile: Option<String>,
) -> Result<GatewayWsUrlResult, String> {
    if let Err(err) = ensure_backend(&state, &app, profile.as_deref()).await {
        return Ok(GatewayWsUrlResult::failed(err));
    }

    let slot = state.backend.lock().await;
    let Some(handle) = slot.as_ref() else {
        return Ok(GatewayWsUrlResult::failed("backend unavailable"));
    };

    Ok(GatewayWsUrlResult {
        ok: true,
        ws_url: Some(backend::gateway_ws_url(&handle.base_url, &handle.token)),
        error: None,
        needs_oauth_login: None,
    })
}

#[tauri::command]
pub async fn hermes_gateway_ws_url_for(
    state: State<'_, AppState>,
    app: AppHandle,
    payload: ConnectionOptionsFor,
) -> Result<GatewayWsUrlResult, String> {
    hermes_gateway_ws_url(state, app, payload.profile).await
}

/// Keepalive. The Tauri shell runs no idle reaper (there is exactly one
/// managed child), so this is an honest no-op that keeps the renderer's
/// fire-and-forget call site working.
#[tauri::command]
pub async fn hermes_backend_touch(_profile: Option<String>) -> Result<OkResult, String> {
    Ok(OkResult { ok: true })
}

/// Tear the backend down so the next `hermes:connection` respawns it. Used by
/// Settings after a config change that needs a fresh process.
#[tauri::command]
pub async fn hermes_backend_recycle(
    state: State<'_, AppState>,
    app: AppHandle,
    profile: Option<String>,
) -> Result<OkResult, String> {
    let mut slot = state.backend.lock().await;

    if let Some(mut handle) = slot.take() {
        backend::stop_backend(&mut handle).await;
    }

    drop(slot);

    // Electron's `resetBootProgressForReconnect`: a recycle re-runs the whole
    // cold start, and the one step that legitimately moves the bar *backwards*.
    boot::advance(
        &state.boot,
        &app,
        Step::rewind("backend.resolve", "Restarting desktop connection", 4),
    );

    ensure_backend(&state, &app, profile.as_deref()).await?;

    Ok(OkResult { ok: true })
}

/// The one HTTP path between renderer and backend.
///
/// The renderer cannot fetch the backend directly: the webview's origin is not
/// the backend's, so a direct call is a cross-origin request the backend does
/// not answer. Proxying through Rust also injects the session token, which the
/// renderer must never hold for a remote connection.
#[tauri::command]
pub async fn hermes_api(
    state: State<'_, AppState>,
    app: AppHandle,
    request: ApiRequest,
) -> Result<serde_json::Value, String> {
    let method_text = request
        .method
        .clone()
        .unwrap_or_else(|| "GET".to_string())
        .to_uppercase();
    logging::info(&format!("[api] -> {method_text} {}", request.path));

    ensure_backend(&state, &app, request.profile.as_deref()).await?;

    let (base_url, token) = {
        let slot = state.backend.lock().await;
        let handle = slot
            .as_ref()
            .ok_or_else(|| "backend unavailable".to_string())?;

        (handle.base_url.clone(), handle.token.clone())
    };

    let url = format!("{}{}", base_url.trim_end_matches('/'), request.path);

    let method = reqwest::Method::from_bytes(method_text.as_bytes())
        .map_err(|err| format!("invalid HTTP method {method_text}: {err}"))?;

    let mut builder = state
        .http
        .request(method, &url)
        .header("X-Hermes-Session-Token", &token)
        .header("Accept", "application/json");

    if let Some(timeout_ms) = request.timeout_ms {
        builder = builder.timeout(Duration::from_millis(timeout_ms));
    }

    if let Some(body) = request.body {
        builder = builder.json(&body);
    }

    let response = builder
        .send()
        .await
        .map_err(|err| format!("request to {url} failed: {err}"))?;

    let status = response.status();
    logging::info(&format!("[api] {} {} -> {}", method_text, request.path, status.as_u16()));

    let text = response
        .text()
        .await
        .map_err(|err| format!("failed to read the response from {url}: {err}"))?;

    if !status.is_success() {
        // Prefer the backend's own message; FastAPI uses `detail`, some routes
        // use `error`. Falling back to a truncated body beats the bare status.
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("detail")
                    .or_else(|| value.get("error"))
                    .and_then(|inner| inner.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.chars().take(400).collect());

        return Err(format!("{} {}", status.as_u16(), detail));
    }

    if text.is_empty() {
        return Ok(serde_json::Value::Null);
    }

    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => Ok(value),
        // A non-JSON 200 (a plain-text export, an SSE body) is returned as-is
        // rather than being downgraded to an error.
        Err(_) => Ok(serde_json::Value::String(text)),
    }
}

#[tauri::command]
pub async fn hermes_launch_flags() -> Result<LaunchFlags, String> {
    Ok(LaunchFlags {
        local_models: std::env::var("HERMES_DESKTOP_LOCAL_MODELS")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
    })
}

#[tauri::command]
pub async fn hermes_pool_limits_get() -> Result<PoolLimits, String> {
    Ok(PoolLimits {
        max_backends: 1,
        idle_ms: 0,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolLimits {
    pub max_backends: u32,
    pub idle_ms: u64,
}
