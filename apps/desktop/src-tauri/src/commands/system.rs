//! Shell-level commands: version info, clipboard, external links, native
//! notifications, logs, device-local settings, and the native chrome/power
//! toggles the renderer mirrors into the OS.

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::{DialogExt, FilePath};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::oneshot;

use crate::logging;
use crate::paths;

/// Mirror of `DesktopVersionInfo` (`src/global.d.ts`). The two Electron-specific
/// fields are populated for shape compatibility and flagged by `shell`, so the
/// renderer can branch instead of rendering an empty version string.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopVersionInfo {
    pub app_version: String,
    pub electron_version: String,
    pub node_version: String,
    pub platform: String,
    pub hermes_root: String,
    /// Not part of the Electron type — `"tauri"` here, absent there.
    pub shell: String,
    pub tauri_version: String,
}

/// `process.platform` naming, so renderer branches keyed on `'win32'` /
/// `'darwin'` keep working.
fn platform_name() -> String {
    match std::env::consts::OS {
        "windows" => "win32".to_string(),
        "macos" => "darwin".to_string(),
        other => other.to_string(),
    }
}

#[tauri::command]
pub async fn hermes_version() -> Result<DesktopVersionInfo, String> {
    Ok(DesktopVersionInfo {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        // No V8/Node in this shell. Left as empty strings rather than dropped
        // fields so the renderer's shape checks (`if (version.nodeVersion)`)
        // keep evaluating instead of reading `undefined`.
        electron_version: String::new(),
        node_version: String::new(),
        platform: platform_name(),
        hermes_root: paths::hermes_home().to_string_lossy().to_string(),
        shell: "tauri".to_string(),
        tauri_version: tauri::VERSION.to_string(),
    })
}

#[tauri::command]
pub async fn hermes_app_relaunch(app: AppHandle) -> Result<(), String> {
    logging::info("relaunch requested");

    app.restart();
}

#[tauri::command]
pub async fn hermes_open_external(app: AppHandle, url: String) -> Result<bool, String> {
    // Only hand the OS URLs it can actually resolve; a bare path here would be
    // interpreted as a relative file URL by the opener.
    let trimmed = url.trim();

    if trimmed.is_empty() {
        return Err("refusing to open an empty URL".to_string());
    }

    app.opener()
        .open_url(trimmed, None::<&str>)
        .map_err(|err| format!("cannot open {trimmed}: {err}"))?;

    Ok(true)
}

#[tauri::command]
pub async fn hermes_clipboard_read(app: AppHandle) -> Result<String, String> {
    app.clipboard()
        .read_text()
        .map_err(|err| format!("cannot read the clipboard: {err}"))
}

#[tauri::command]
pub async fn hermes_clipboard_write(app: AppHandle, text: String) -> Result<bool, String> {
    app.clipboard()
        .write_text(text)
        .map_err(|err| format!("cannot write the clipboard: {err}"))?;

    Ok(true)
}

/// `tag` is not carried: Electron used it to replace a prior notification with
/// the same tag, and the Tauri notification plugin has no string-keyed
/// equivalent (its `id` is numeric and identifies a click target). serde drops
/// the key, so callers that still send it are unaffected.
#[derive(Debug, Deserialize)]
pub struct NotifyPayload {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[tauri::command]
pub async fn hermes_notify(app: AppHandle, payload: NotifyPayload) -> Result<bool, String> {
    let title = payload.title.unwrap_or_else(|| "Hermes".to_string());
    let body = payload.body.unwrap_or_default();

    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|err| format!("cannot show a notification: {err}"))?;

    Ok(true)
}

#[tauri::command]
pub async fn hermes_logs_reveal(app: AppHandle) -> Result<bool, String> {
    let dir = paths::logs_dir();

    app.opener()
        .open_path(dir.to_string_lossy().to_string(), None::<&str>)
        .map_err(|err| format!("cannot open {}: {err}", dir.display()))?;

    Ok(true)
}

/// The tail of `desktop.log`, newest last — the shape the in-app log viewer
/// expects from `getRecentLogs()`.
#[tauri::command]
pub async fn hermes_logs_recent() -> Result<Vec<String>, String> {
    let contents = logging::recent(256 * 1024);

    Ok(contents.lines().map(str::to_string).collect())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererErrorReport {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub component_stack: Option<String>,
    #[serde(default)]
    pub stack: Option<String>,
}

/// Fire-and-forget from the renderer's error boundary. Never fails loudly: a
/// crash reporter that throws is worse than one that drops a line.
#[tauri::command]
pub async fn hermes_logs_renderer_error(report: RendererErrorReport) -> Result<(), String> {
    let message = report.message.unwrap_or_else(|| "unknown renderer error".to_string());

    logging::error(&format!("renderer: {message}"));

    if let Some(stack) = report.stack {
        logging::error(&format!("renderer stack: {stack}"));
    }

    if let Some(component_stack) = report.component_stack {
        logging::error(&format!("renderer component stack: {component_stack}"));
    }

    Ok(())
}

/// `userData`-equivalent scratch directory for device-local desktop settings
/// (window geometry, quick-entry config, data-url read cap). Deliberately NOT
/// `HERMES_HOME`: these are device preferences, not agent state, and must not
/// sync with a profile.
fn device_state_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| paths::home_dir().join(".local/share"))
        .join("Hermes")
}

/// Settings → Workspace → default project directory.
///
/// Persisted as a bare path string. Electron keeps `project-dir.json` in
/// `userData`, but that object carries a single field, and nothing outside this
/// shell reads either file.
fn default_project_dir_file() -> std::path::PathBuf {
    device_state_dir().join("default-project-dir.txt")
}

/// Electron's `app.getPath('home')`, as the renderer's `defaultLabel`.
fn home_dir_string() -> String {
    paths::home_dir().to_string_lossy().to_string()
}

/// The persisted default project directory: `None` when it was never set, holds
/// only whitespace, or names a directory that is since gone.
///
/// The existence check mirrors `readDefaultProjectDir()` (`electron/main.ts`).
/// Without it a directory the user deleted keeps coming back as the destination
/// for new sessions — the setting points at nothing and the failure surfaces
/// later, as files landing in the fallback.
///
/// `state` arrives as an argument rather than being derived here so a test can
/// aim it at a scratch directory; the commands pass `default_project_dir_file()`.
fn read_default_project_dir(state: &std::path::Path) -> Option<String> {
    let stored = std::fs::read_to_string(state).ok()?;
    let trimmed = stored.trim();

    if trimmed.is_empty() {
        return None;
    }

    std::path::Path::new(trimmed)
        .is_dir()
        .then(|| trimmed.to_string())
}

/// Persist the setting, or clear it when `dir` is `None`.
///
/// A failed write is an error rather than the log line Electron settles for:
/// the renderer shows a success toast on return, so swallowing would tell the
/// user a preference was saved that was not.
fn write_default_project_dir(state: &std::path::Path, dir: Option<&str>) -> Result<(), String> {
    if let Some(parent) = state.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }

    // An empty file is how "cleared" reads back.
    std::fs::write(state, dir.unwrap_or_default())
        .map_err(|err| format!("could not save the project directory: {err}"))
}

/// Where a new session would start — Electron's `resolveHermesCwd()`.
///
/// That chain also weighed `HERMES_DESKTOP_CWD`, `INIT_CWD`, `process.cwd()`
/// and the source root, all of them ways to keep a *dev* Electron run out of its
/// own install directory (`win-unpacked`, a `.app` bundle). This shell is not
/// launched from the tree it ships in — `hermes_root` comes from `HERMES_HOME`
/// (`paths.rs`) — so only the two candidates that still mean something remain.
fn resolve_default_cwd(default_dir: Option<&str>) -> String {
    default_dir
        .map(str::to_string)
        .unwrap_or_else(home_dir_string)
}

/// Mirror of the renderer's `settings.getDefaultProjectDir()` contract
/// (`src/global.d.ts`): `{ defaultLabel, dir, resolvedCwd }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultProjectDirState {
    /// Fallback shown when nothing is configured — the home directory.
    pub default_label: String,
    /// The configured directory, or `null` when unset or no longer present.
    pub dir: Option<String>,
    /// Where a new session would start.
    pub resolved_cwd: String,
}

#[tauri::command]
pub async fn hermes_setting_default_project_dir_get() -> Result<DefaultProjectDirState, String> {
    let dir = read_default_project_dir(&default_project_dir_file());
    let resolved_cwd = resolve_default_cwd(dir.as_deref());

    Ok(DefaultProjectDirState {
        default_label: home_dir_string(),
        dir,
        resolved_cwd,
    })
}

/// Mirror of the renderer's `settings.setDefaultProjectDir(dir)` contract
/// (`src/global.d.ts`): `{ dir }`, echoing what was stored.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultProjectDirUpdate {
    pub dir: Option<String>,
}

/// `dir` is `Option<String>` rather than `String` because the renderer's Clear
/// button sends `null` — a plain `String` fails deserialization, so "clear"
/// could only ever error.
#[tauri::command]
pub async fn hermes_setting_default_project_dir_set(
    dir: Option<String>,
) -> Result<DefaultProjectDirUpdate, String> {
    let next = dir
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(next) = &next {
        // The picker hands back an existing folder, but a restored or typed
        // path may not be there yet. Electron creates it for the same reason.
        std::fs::create_dir_all(next)
            .map_err(|err| format!("Could not create directory: {err}"))?;
    }

    write_default_project_dir(&default_project_dir_file(), next.as_deref())?;

    Ok(DefaultProjectDirUpdate { dir: next })
}

/// Mirror of the renderer's `settings.pickDefaultProjectDir()` contract
/// (`src/global.d.ts`): `{ canceled, dir }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultProjectDirPick {
    pub canceled: bool,
    pub dir: Option<String>,
}

/// Folder picker for the setting, starting at the configured directory and
/// falling back to home, as the Electron handler's `defaultPath` did.
///
/// Reuses the picker plumbing behind `hermes_select_paths` instead of a second
/// dialog implementation. No title is set: Electron hardcoded an English
/// "Choose default project directory", and the renderer has no localized string
/// to hand down here, so the OS default beats an untranslatable literal.
#[tauri::command]
pub async fn hermes_setting_default_project_dir_pick(
    app: AppHandle,
) -> Result<DefaultProjectDirPick, String> {
    let default_path = read_default_project_dir(&default_project_dir_file())
        .unwrap_or_else(home_dir_string);
    let builder = configure_dialog(app.dialog().file(), None, Some(default_path), None);
    let (tx, rx) = oneshot::channel::<Option<FilePath>>();

    builder.pick_folder(move |path| {
        let _ = tx.send(path);
    });

    let picked = rx
        .await
        .map_err(|_| "the folder picker closed without returning a selection".to_string())?;
    let dir = file_paths_to_strings(picked.map(|path| vec![path]))
        .into_iter()
        .next();

    Ok(DefaultProjectDirPick {
        canceled: dir.is_none(),
        dir,
    })
}

/// Renderer-side shape of `selectPaths(options)` / `selectSavePath(options)`
/// (`electron/preload.ts`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathPickerOptions {
    /// `selectPaths` only: pick folders instead of files.
    #[serde(default)]
    pub directories: bool,
    /// `selectPaths` only. Absent means multi-select, matching the preload's
    /// `options?.multiple !== false`.
    #[serde(default)]
    pub multiple: Option<bool>,
    #[serde(default)]
    pub default_path: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub filters: Option<Vec<PathPickerFilter>>,
}

#[derive(Debug, Deserialize)]
pub struct PathPickerFilter {
    pub name: String,
    #[serde(default)]
    pub extensions: Vec<String>,
}

fn configure_dialog<R: tauri::Runtime>(
    builder: tauri_plugin_dialog::FileDialogBuilder<R>,
    title: Option<String>,
    default_path: Option<String>,
    filters: Option<Vec<PathPickerFilter>>,
) -> tauri_plugin_dialog::FileDialogBuilder<R> {
    let mut builder = builder;

    if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
        builder = builder.set_title(title);
    }

    if let Some(default_path) = default_path.filter(|value| !value.trim().is_empty()) {
        builder = builder.set_directory(default_path);
    }

    for filter in filters.unwrap_or_default() {
        let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();

        builder = builder.add_filter(filter.name, &extensions);
    }

    builder
}

fn file_paths_to_strings(paths: Option<Vec<FilePath>>) -> Vec<String> {
    paths
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().to_string())
        .collect()
}

/// Native open dialog. Returns the selection as absolute paths; an empty array
/// means the user cancelled, which is what the preload's `result.filePaths`
/// produced.
#[tauri::command]
pub async fn hermes_select_paths(
    app: AppHandle,
    options: Option<PathPickerOptions>,
) -> Result<Vec<String>, String> {
    let PathPickerOptions {
        directories,
        multiple,
        default_path,
        title,
        filters,
    } = options.unwrap_or_default();

    // Absent `multiple` means "yes", matching `options?.multiple !== false`.
    let multiple = multiple.unwrap_or(true);
    let builder = configure_dialog(app.dialog().file(), title, default_path, filters);

    let (tx, rx) = oneshot::channel::<Option<Vec<FilePath>>>();

    if directories {
        if multiple {
            builder.pick_folders(move |paths| {
                let _ = tx.send(paths);
            });
        } else {
            builder.pick_folder(move |path| {
                let _ = tx.send(path.map(|path| vec![path]));
            });
        }
    } else if multiple {
        builder.pick_files(move |paths| {
            let _ = tx.send(paths);
        });
    } else {
        builder.pick_file(move |path| {
            let _ = tx.send(path.map(|path| vec![path]));
        });
    }

    let picked = rx
        .await
        .map_err(|_| "the file picker closed without returning a selection".to_string())?;

    Ok(file_paths_to_strings(picked))
}

/// Native save dialog. `None` means the user cancelled, matching the preload's
/// `null` return.
#[tauri::command]
pub async fn hermes_select_save_path(
    app: AppHandle,
    options: Option<PathPickerOptions>,
) -> Result<Option<String>, String> {
    let PathPickerOptions {
        default_path,
        title,
        filters,
        ..
    } = options.unwrap_or_default();

    let builder = configure_dialog(app.dialog().file(), title, default_path, filters);
    let (tx, rx) = oneshot::channel::<Option<FilePath>>();

    builder.save_file(move |path| {
        let _ = tx.send(path);
    });

    let picked = rx
        .await
        .map_err(|_| "the save dialog closed without returning a path".to_string())?;

    Ok(picked
        .and_then(|path| path.into_path().ok())
        .map(|path| path.to_string_lossy().to_string()))
}

// ---------------------------------------------------------------- native chrome

/// Pin the native window chrome — title bar, scrollbars, system dialogs — to the
/// app's theme. Electron's `nativeTheme.themeSource`.
///
/// `system` is the absence of a pin rather than a third mode: it hands control
/// back to the OS, which is also Tauri's default. Anything unrecognized is
/// treated the same way, so a mode name this shell does not know can never leave
/// the chrome stuck on a stale theme.
#[tauri::command]
pub async fn hermes_native_theme_set(
    app: AppHandle,
    mode: Option<String>,
) -> Result<Option<String>, String> {
    let theme = match mode.as_deref().map(str::trim) {
        Some("dark") => Some(tauri::Theme::Dark),
        Some("light") => Some(tauri::Theme::Light),
        _ => None,
    };

    app.set_theme(theme);

    Ok(mode)
}

/// Keep the display awake for as long as a turn is running — Electron's
/// `powerSaveBlocker.start('prevent-display-sleep')`.
///
/// `ES_CONTINUOUS` makes the request stick until it is cleared, but it is still
/// *per-thread* state, and a command handler runs on a tokio worker that is
/// recycled the moment it returns. The call is therefore bounced onto the main
/// thread, which outlives every handler.
///
/// Nothing is persisted here. The Electron main process kept its own copy so a
/// cold launch could restore the blocker, but the renderer's `$keepAwake`
/// subscription (`src/store/keep-awake.ts`) fires immediately with the stored
/// value on every launch, so the toggle re-applies itself.
#[tauri::command]
pub async fn hermes_keep_awake_set(app: AppHandle, on: bool) -> Result<bool, String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
        };

        // The display request comes with the system one: a display cannot stay
        // lit on a system that is still allowed to sleep.
        let flags = if on {
            ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED
        } else {
            ES_CONTINUOUS
        };

        app.run_on_main_thread(move || {
            // Returns the *previous* state, or 0 when the call was rejected.
            if unsafe { SetThreadExecutionState(flags) } == 0 {
                logging::warn("the keep-awake request was rejected by the system");
            }
        })
        .map_err(|err| format!("could not reach the main thread: {err}"))?;
    }

    #[cfg(not(windows))]
    {
        let _ = &app;

        logging::warn("keep-awake is not implemented on this platform");
    }

    Ok(on)
}

/// Whether the machine is drawing from its battery — Electron's
/// `powerMonitor.onBatteryPower`. The renderer stretches its backstop poll
/// intervals on battery, so an idle chat window stops waking a laptop up.
///
/// `ACLineStatus` is 1 on AC, 0 on battery, and 255 when the status is unknown.
/// Only an explicit 0 counts, so a status that cannot be read keeps the fast
/// polls instead of silently slowing a desktop down. A query that never fails is
/// also a query whose failure the renderer cannot mishandle — it discards the
/// promise.
#[tauri::command]
pub async fn hermes_power_battery_get() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

        // All-primitive bindings; `zeroed` avoids depending on a `Default` impl
        // the generated structs do not carry.
        let mut status: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };

        if unsafe { GetSystemPowerStatus(&mut status) } == 0 {
            return false;
        }

        status.ACLineStatus == 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

// -------------------------------------------------------------- remote display

/// `HERMES_DESKTOP_DISABLE_GPU` values that force the check either way. Ported
/// from `GPU_OVERRIDE_ON` / `GPU_OVERRIDE_OFF` in `electron/bootstrap-platform.ts`.
const GPU_OVERRIDE_ON: [&str; 4] = ["1", "true", "yes", "on"];
const GPU_OVERRIDE_OFF: [&str; 4] = ["0", "false", "no", "off"];

/// The reason this display counts as remote, or `None` to keep the GPU on.
///
/// Port of `detectRemoteDisplay()` (`electron/bootstrap-platform.ts`) — the same
/// four checks in the same order, because the string it returns reaches the user
/// verbatim in the renderer's software-rendering banner. The environment arrives
/// as a lookup rather than being read here, so a test can hand it a table
/// instead of mutating the process it runs in.
pub(crate) fn remote_display_reason(
    lookup: impl Fn(&str) -> Option<String>,
    platform: &str,
) -> Option<String> {
    // `String(env.X || '')` in the original: an empty variable is an unset one.
    let set = |value: Option<String>| value.filter(|entry| !entry.is_empty());

    let override_value = set(lookup("HERMES_DESKTOP_DISABLE_GPU"))
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    if GPU_OVERRIDE_ON.contains(&override_value.as_str()) {
        return Some("override (HERMES_DESKTOP_DISABLE_GPU)".to_string());
    }

    // Forced off beats every detection below, which is the only way to opt out on
    // a remote display that renders fine.
    if GPU_OVERRIDE_OFF.contains(&override_value.as_str()) {
        return None;
    }

    // A forwarded SSH session means the pixels are being shipped elsewhere.
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|key| set(lookup(*key)).is_some())
    {
        return Some("ssh-session".to_string());
    }

    if platform == "linux" {
        // X11 sets DISPLAY to "<host>:<n>" when forwarding and to ":<n>" from the
        // local server, so a host part is the tell. WSLg is deliberately not
        // caught: it presents a local display.
        let display = set(lookup("DISPLAY")).unwrap_or_default();
        let host = display.split(':').next().unwrap_or_default();

        if display.contains(':') && !host.is_empty() {
            return Some(format!("x11-forwarding (DISPLAY={display})"));
        }
    }

    if platform == "win32" {
        // An RDP session is named "RDP-Tcp#<n>"; the local console is "Console".
        let session = set(lookup("SESSIONNAME")).unwrap_or_default();

        if session.to_lowercase().starts_with("rdp-") {
            return Some(format!("rdp (SESSIONNAME={session})"));
        }
    }

    None
}

/// The switch this platform uses to fall back to software rendering.
///
/// Chromium had one spelling for all three platforms; the webviews this shell
/// drives do not. macOS is the one platform with no switch at all — WKWebView
/// composites through the window server rather than owning a GPU process — which
/// is why the command below reports nothing there.
fn gpu_fallback_switch() -> Option<(&'static str, &'static str)> {
    match std::env::consts::OS {
        "windows" => Some(("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", "--disable-gpu")),
        "linux" => Some(("WEBKIT_DISABLE_COMPOSITING_MODE", "1")),
        _ => None,
    }
}

/// Leave the GPU off when the display is remote.
///
/// Called before the webview exists, not from a command: WebView2 reads its
/// extra switches when it creates the environment, and nothing can reach back
/// that far.
pub(crate) fn apply_remote_display_fallback() {
    let Some(reason) = remote_display_reason(|key| std::env::var(key).ok(), &platform_name()) else {
        return;
    };

    let Some((key, value)) = gpu_fallback_switch() else {
        logging::warn(&format!(
            "remote display detected ({reason}) but this platform has no software-rendering switch"
        ));

        return;
    };

    // Appended, never replaced: the user may already be passing switches of
    // their own, and one of them may be this very one.
    let existing = std::env::var(key).unwrap_or_default();

    if !existing.split_whitespace().any(|part| part == value) {
        let merged = if existing.trim().is_empty() {
            value.to_string()
        } else {
            format!("{existing} {value}")
        };

        std::env::set_var(key, merged);
    }

    logging::info(&format!("software rendering enabled ({reason})"));
}

/// Why GPU acceleration is off, for the renderer's banner
/// (`src/components/remote-display-banner.tsx`). `None` when it is on.
///
/// The disabling already happened before this window existed (see
/// `apply_remote_display_fallback`), so this can only report it — which is also
/// why a platform with no switch reports nothing: the banner tells the user that
/// software rendering is in use, and it must not say that where nothing changed.
#[tauri::command]
pub async fn hermes_get_remote_display_reason() -> Result<Option<String>, String> {
    if gpu_fallback_switch().is_none() {
        return Ok(None);
    }

    Ok(remote_display_reason(
        |key| std::env::var(key).ok(),
        &platform_name(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch stand-in for the device state file. `device_state_dir()` reads
    /// the real `%LOCALAPPDATA%\Hermes` (or `~/.local/share/Hermes`), which a
    /// test must not touch, so the helpers take the path instead. Same temp-dir
    /// convention as `commands/fs.rs`.
    fn scratch_file() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!(
                "hermes-default-project-dir-{}",
                uuid::Uuid::new_v4()
            ))
            .join("default-project-dir.txt")
    }

    fn sibling_dir(state: &std::path::Path) -> std::path::PathBuf {
        let project = state.parent().unwrap().join("project");

        std::fs::create_dir_all(&project).unwrap();

        project
    }

    fn clean_up(state: &std::path::Path) {
        std::fs::remove_dir_all(state.parent().unwrap()).ok();
    }

    #[test]
    fn a_saved_directory_reads_back() {
        let state = scratch_file();
        let project = sibling_dir(&state);

        write_default_project_dir(&state, Some(project.to_str().unwrap())).unwrap();

        assert_eq!(read_default_project_dir(&state).as_deref(), project.to_str());

        clean_up(&state);
    }

    #[test]
    fn a_directory_that_is_gone_reads_as_unset() {
        let state = scratch_file();
        let project = sibling_dir(&state);

        write_default_project_dir(&state, Some(project.to_str().unwrap())).unwrap();

        // Retiring a deleted folder is the point of the existence check: without
        // it the renderer keeps aiming new sessions at a path that is not there,
        // and the user only finds out when files land somewhere else.
        std::fs::remove_dir_all(&project).unwrap();

        assert_eq!(read_default_project_dir(&state), None);

        clean_up(&state);
    }

    #[test]
    fn clearing_leaves_nothing_to_read_back() {
        let state = scratch_file();
        let project = sibling_dir(&state);

        write_default_project_dir(&state, Some(project.to_str().unwrap())).unwrap();
        write_default_project_dir(&state, None).unwrap();

        assert_eq!(read_default_project_dir(&state), None);

        clean_up(&state);
    }

    #[test]
    fn a_blank_value_is_not_a_setting() {
        let state = scratch_file();

        write_default_project_dir(&state, Some("   ")).unwrap();

        assert_eq!(read_default_project_dir(&state), None);

        clean_up(&state);
    }

    /// A configured directory wins; with nothing configured, a new session
    /// starts at home.
    #[test]
    fn resolved_cwd_falls_back_to_home() {
        assert_eq!(resolve_default_cwd(Some("/work")), "/work");
        assert_eq!(resolve_default_cwd(None), home_dir_string());
    }

    /// These key names are the contract, not an implementation detail: the
    /// renderer destructures `{ defaultLabel, dir, resolvedCwd }`
    /// (`src/global.d.ts`). Drop `serde(rename_all = "camelCase")` and the
    /// payload ships `default_label`, every field reads back `undefined`, and the
    /// setting is quietly dead again — the exact failure this port was fixing, so
    /// it is worth failing a test over rather than rediscovering in the UI.
    #[test]
    fn the_get_payload_carries_the_keys_the_renderer_reads() {
        let json = serde_json::to_value(DefaultProjectDirState {
            default_label: "/home/ada".to_string(),
            dir: Some("/home/ada/work".to_string()),
            resolved_cwd: "/home/ada/work".to_string(),
        })
        .unwrap();

        for key in ["dir", "defaultLabel", "resolvedCwd"] {
            assert!(json.get(key).is_some(), "missing `{key}` in {json}");
        }

        assert_eq!(
            json.as_object().unwrap().len(),
            3,
            "unexpected extra keys in {json}"
        );
    }

    /// Clearing has to arrive as an explicit `null` under `dir`. An omitted key
    /// would read as `undefined` in the renderer, which is a state it never
    /// handles: the setting would look untouched instead of cleared.
    #[test]
    fn clearing_serializes_as_an_explicit_null() {
        let json = serde_json::to_value(DefaultProjectDirUpdate { dir: None }).unwrap();

        assert_eq!(json, serde_json::json!({ "dir": null }));
    }

    /// `remote_display_reason` against a fixed environment.
    fn reason_with(platform: &str, pairs: &[(&str, &str)]) -> Option<String> {
        remote_display_reason(
            |key| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_string())
            },
            platform,
        )
    }

    /// The string this returns is shown to the user verbatim, so each tell has to
    /// name itself the way the banner reads it — and one has to be reachable on a
    /// machine no check catches, which is what the override is for.
    #[test]
    fn a_remote_display_is_named_the_way_the_banner_shows_it() {
        assert_eq!(reason_with("win32", &[]), None);
        assert_eq!(reason_with("darwin", &[]), None);

        // RDP names the session; the local console is "Console" and is not one,
        // and the comparison is case-insensitive like the `/^rdp-/i` it replaces.
        assert_eq!(
            reason_with("win32", &[("SESSIONNAME", "RDP-Tcp#7")]).as_deref(),
            Some("rdp (SESSIONNAME=RDP-Tcp#7)")
        );
        assert_eq!(
            reason_with("win32", &[("SESSIONNAME", "rdp-tcp#7")]).as_deref(),
            Some("rdp (SESSIONNAME=rdp-tcp#7)")
        );
        assert_eq!(reason_with("win32", &[("SESSIONNAME", "Console")]), None);

        // A session name only means RDP on Windows.
        assert_eq!(reason_with("linux", &[("SESSIONNAME", "RDP-Tcp#7")]), None);

        assert_eq!(
            reason_with("win32", &[("SSH_CONNECTION", "10.0.0.1 5 10.0.0.2 22")]).as_deref(),
            Some("ssh-session")
        );
        assert_eq!(
            reason_with("win32", &[("SSH_TTY", "/dev/pts/1")]).as_deref(),
            Some("ssh-session")
        );

        // An empty variable is an unset one — the original read it as
        // `String(env.X || '')`, so a `Some("")` must not count as a tell.
        assert_eq!(reason_with("win32", &[("SSH_CONNECTION", "")]), None);

        // X11 forwarding carries a host before the colon; a local server is ":0".
        assert_eq!(
            reason_with("linux", &[("DISPLAY", "localhost:10.0")]).as_deref(),
            Some("x11-forwarding (DISPLAY=localhost:10.0)")
        );
        assert_eq!(reason_with("linux", &[("DISPLAY", ":0")]), None);
        assert_eq!(reason_with("linux", &[("DISPLAY", "")]), None);

        // …and X11 is a Linux question.
        assert_eq!(
            reason_with("win32", &[("DISPLAY", "localhost:10.0")]),
            None
        );
    }

    /// Both directions of the escape hatch, and the values that are not it.
    #[test]
    fn the_gpu_override_outranks_every_detection() {
        // Forcing it off has to work where no check fires.
        assert_eq!(
            reason_with("win32", &[("HERMES_DESKTOP_DISABLE_GPU", "1")]).as_deref(),
            Some("override (HERMES_DESKTOP_DISABLE_GPU)")
        );
        // Spelled loosely, like the docs' "yes"/"on" — trimmed and case-folded.
        assert_eq!(
            reason_with("win32", &[("HERMES_DESKTOP_DISABLE_GPU", " YES ")]).as_deref(),
            Some("override (HERMES_DESKTOP_DISABLE_GPU)")
        );

        // …and forcing it back on has to beat a real detection, or a user whose
        // remote display renders fine has no way out.
        assert_eq!(
            reason_with(
                "win32",
                &[
                    ("HERMES_DESKTOP_DISABLE_GPU", "0"),
                    ("SESSIONNAME", "RDP-Tcp#7")
                ]
            ),
            None
        );
        assert_eq!(
            reason_with(
                "linux",
                &[("HERMES_DESKTOP_DISABLE_GPU", "off"), ("SSH_CONNECTION", "x")]
            ),
            None
        );

        // Absent, blank and unrecognized all mean "decide it yourself".
        assert_eq!(reason_with("win32", &[("HERMES_DESKTOP_DISABLE_GPU", "")]), None);
        assert_eq!(
            reason_with(
                "win32",
                &[
                    ("HERMES_DESKTOP_DISABLE_GPU", "maybe"),
                    ("SESSIONNAME", "RDP-Tcp#7")
                ]
            )
            .as_deref(),
            Some("rdp (SESSIONNAME=RDP-Tcp#7)")
        );
    }
}
