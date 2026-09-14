//! Window creation and lifecycle.
//!
//! The renderer URL contract is the load-bearing part and is preserved exactly:
//! `?win=<kind>` goes in the **search** string, **before** the `#`, because the
//! app uses a HashRouter and a query after the `#` would be swallowed as part
//! of the route (see `electron/browser-windows.ts`, `electron/hud-url.ts`).

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::logging;

/// Secondary windows are ordinary app windows, not OS chrome: they need a
/// title bar and a resizable frame (the Electron shell used the same defaults
/// for session windows).
const SECONDARY_MIN_WIDTH: f64 = 420.0;
const SECONDARY_MIN_HEIGHT: f64 = 320.0;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowOpenResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl WindowOpenResult {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct SessionWindowOptions {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub watch: Option<bool>,
}

/// Build the renderer URL for a secondary window.
///
/// `kind` is the `win` query value the renderer switches on; `extra_query` is
/// appended inside the search string, before the hash.
pub fn renderer_url(kind: Option<&str>, extra_query: &[(&str, &str)], route: Option<&str>) -> String {
    let mut query = String::new();

    if let Some(kind) = kind.filter(|value| !value.is_empty()) {
        query.push_str(&format!("win={kind}"));
    }

    for (key, value) in extra_query {
        if value.is_empty() {
            continue;
        }

        if !query.is_empty() {
            query.push('&');
        }

        query.push_str(&format!("{key}={}", encode_query_component(value)));
    }

    let mut url = String::from("index.html");

    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }

    url.push_str(&format!("#{}", route.unwrap_or("/")));

    url
}

/// Percent-encode for a query value, matching `encodeURIComponent` for the
/// characters that realistically appear in session ids and profile names.
fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());

    for byte in value.bytes() {
        let ch = byte as char;

        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }

    out
}

async fn show_main_window(app: &AppHandle) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("the main window is missing".to_string());
    };

    window.show().map_err(|err| format!("failed to show the main window: {err}"))?;
    window
        .set_focus()
        .map_err(|err| format!("failed to focus the main window: {err}"))?;

    Ok(())
}

/// The renderer calls this once it has mounted.
///
/// The main window is declared `visible: false` so the user never sees a blank
/// webview flash before first paint; showing it is the renderer's call, since
/// only it knows when there is something worth showing.
#[tauri::command]
pub async fn hermes_window_ready(app: AppHandle) -> Result<WindowOpenResult, String> {
    match show_main_window(&app).await {
        Ok(()) => Ok(WindowOpenResult::ok()),
        Err(err) => {
            logging::warn(&err);

            Ok(WindowOpenResult::failed(err))
        }
    }
}

#[tauri::command]
pub async fn hermes_window_open_instance(app: AppHandle) -> Result<WindowOpenResult, String> {
    open_secondary(&app, "instance", None, &[], None).await
}

#[tauri::command]
pub async fn hermes_window_open_session(
    app: AppHandle,
    session_id: String,
    opts: Option<SessionWindowOptions>,
) -> Result<WindowOpenResult, String> {
    let session_id = session_id.trim().to_string();

    if session_id.is_empty() {
        return Ok(WindowOpenResult::failed("empty sessionId"));
    }

    let opts = opts.unwrap_or_default();
    let profile = opts.profile.unwrap_or_default();
    let mut extra: Vec<(&str, &str)> = vec![("session", session_id.as_str())];

    if !profile.is_empty() {
        extra.push(("profile", profile.as_str()));
    }

    if opts.watch == Some(true) {
        extra.push(("watch", "1"));
    }

    let route = session_route(&session_id);

    open_secondary(&app, "session", Some(route.as_str()), &extra, None).await
}

/// The hash route for a session window.
///
/// Matches Electron's `buildSessionWindowUrl`: `#/${encodeURIComponent(id)}` —
/// the whole id becomes **one** encoded path segment, so an id containing `/`
/// cannot escape into a deeper route. Encoding here rather than at the call site
/// is what makes the behavior testable without building a window.
fn session_route(session_id: &str) -> String {
    format!("/{}", encode_query_component(session_id))
}

#[tauri::command]
pub async fn hermes_window_open_browser(app: AppHandle, tab_id: Option<String>) -> Result<WindowOpenResult, String> {
    let tab = tab_id.unwrap_or_default();
    let extra: Vec<(&str, &str)> = if tab.trim().is_empty() {
        Vec::new()
    } else {
        vec![("tab", tab.trim())]
    };

    open_secondary(&app, "browser", None, &extra, Some((960.0, 720.0, 480.0, 400.0))).await
}

/// The window chrome the renderer needs to lay out its title bar. Kept as a
/// struct (not a bare bool) so fields can be added without touching call sites.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowState {
    pub is_fullscreen: bool,
}

#[tauri::command]
pub async fn hermes_window_state(app: AppHandle) -> Result<WindowState, String> {
    let fullscreen = app
        .get_webview_window("main")
        .and_then(|window| window.is_fullscreen().ok())
        .unwrap_or(false);

    Ok(WindowState {
        is_fullscreen: fullscreen,
    })
}

async fn open_secondary(
    app: &AppHandle,
    kind: &str,
    route: Option<&str>,
    extra_query: &[(&str, &str)],
    size: Option<(f64, f64, f64, f64)>,
) -> Result<WindowOpenResult, String> {
    let url = renderer_url(Some(kind), extra_query, route);
    let label = format!("{kind}-{}", uuid::Uuid::new_v4());

    let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
        .title("Hermes")
        .min_inner_size(SECONDARY_MIN_WIDTH, SECONDARY_MIN_HEIGHT)
        .resizable(true)
        .decorations(true);

    builder = match size {
        Some((width, height, _, _)) => builder.inner_size(width, height),
        None => builder.inner_size(1120.0, 780.0),
    };

    match builder.build() {
        Ok(window) => {
            let _ = window.set_focus();

            Ok(WindowOpenResult::ok())
        }
        Err(err) => {
            let message = format!("failed to open a {kind} window: {err}");
            logging::warn(&message);

            Ok(WindowOpenResult::failed(message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_sits_before_the_hash() {
        let url = renderer_url(Some("browser"), &[("tab", "url:browser-1")], None);

        assert_eq!(url, "index.html?win=browser&tab=url%3Abrowser-1#/");
        assert!(url.find("?win=browser").unwrap() < url.find('#').unwrap());
    }

    #[test]
    fn blank_extra_values_are_omitted() {
        let url = renderer_url(Some("session"), &[("profile", ""), ("session", "abc")], Some("/abc"));
        assert_eq!(url, "index.html?win=session&session=abc#/abc");
    }

    #[test]
    fn no_kind_yields_a_bare_app_url() {
        assert_eq!(renderer_url(None, &[], None), "index.html#/");
    }

    #[test]
    fn route_values_are_encoded_as_one_segment() {
        // A session id containing `/` must stay a single route segment — the
        // renderer splits the hash on `/` to find the session.
        assert_eq!(session_route("a b/c"), "/a%20b%2Fc");

        let url = renderer_url(Some("hud"), &[("profile", "my profile")], Some("/a%20b%2Fc"));

        assert_eq!(url, "index.html?win=hud&profile=my%20profile#/a%20b%2Fc");
    }
}
