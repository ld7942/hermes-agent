//! Lifecycle of the desktop-managed Hermes backend process.
//!
//! Replaces the spawn/claim/health/teardown block in `electron/main.ts`
//! (~12,500-13,400 there). The contract the renderer depends on is unchanged:
//! spawn `hermes serve --port 0`, read the announced port from stdout, probe
//! `/api/health` (falling back to `/api/status` for backends that predate the
//! health route), and hand back a `HermesConnection`-shaped descriptor.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::backend::{command, env, ready};
use crate::logging;
use crate::paths;

/// The spawned backend plus everything needed to talk to it and to stop it.
///
/// The announced port lives inside `base_url` rather than as its own field —
/// every consumer wants the URL, and a second copy of the number is one more
/// thing that can drift.
pub struct BackendHandle {
    pub child: Child,
    pub pid: u32,
    pub base_url: String,
    pub profile: Option<String>,
    pub token: String,
    /// Retained child output, newest last. Kept past startup so the descriptor
    /// can hand the renderer the same `logs` tail the Electron shell exposed.
    pub output: Arc<Mutex<String>>,
}

/// `{ x, y } | null` on the TS side.
#[derive(Debug, Clone, Serialize)]
pub struct WindowButtonPosition {
    pub x: f64,
    pub y: f64,
}

/// The Rust mirror of the renderer's `HermesConnection` interface
/// (`src/global.d.ts`). Field names are camelCase to match it exactly — the
/// renderer reads this object directly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionDescriptor {
    pub base_url: String,
    pub is_fullscreen: bool,
    pub native_overlay_width: f64,
    pub token: String,
    pub ws_url: String,
    pub logs: Vec<String>,
    /// Only ever `local` or `remote`; a `cloud` saved config resolves to a
    /// `remote` connection under the hood, so this never carries `cloud`.
    pub mode: String,
    pub profile: String,
    pub source: String,
    pub window_button_position: Option<WindowButtonPosition>,
}

/// Cap on retained child output. The RDY sentinel is always near the head on a
/// healthy boot, and an unbounded buffer on a chatty/failing backend would
/// grow without limit for the life of the process.
const MAX_OUTPUT_BUFFER_BYTES: usize = 256 * 1024;

const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const BACKEND_READY_TIMEOUT: Duration = Duration::from_secs(45);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Windows: suppress the console window the child would otherwise flash.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn spawn_output_pump<R>(mut reader: R, sink: Arc<Mutex<String>>, label: &'static str)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];

        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();

                    {
                        let mut guard = sink.lock().await;
                        guard.push_str(&chunk);

                        if guard.len() > MAX_OUTPUT_BUFFER_BYTES {
                            // Keep the tail: the most recent lines are what a
                            // failure report needs.
                            let keep_from = guard.len() - MAX_OUTPUT_BUFFER_BYTES / 2;
                            let keep_from = guard
                                .char_indices()
                                .find(|(idx, _)| *idx >= keep_from)
                                .map(|(idx, _)| idx)
                                .unwrap_or(0);
                            *guard = guard[keep_from..].to_string();
                        }
                    }

                    let trimmed = chunk.trim_end();

                    if !trimmed.is_empty() {
                        logging::append(&format!("backend[{label}] {trimmed}"));
                    }
                }
            }
        }
    });
}

async fn output_tail(output: &Arc<Mutex<String>>) -> String {
    let guard = output.lock().await;
    let text = guard.trim();

    if text.is_empty() {
        return String::new();
    }

    let start = text.len().saturating_sub(2000);
    let start = text
        .char_indices()
        .find(|(idx, _)| *idx >= start)
        .map(|(idx, _)| idx)
        .unwrap_or(0);

    format!("\n--- backend output ---\n{}", &text[start..])
}

/// Poll the child's stdout buffer for the READY sentinel until it appears, the
/// child exits, or the deadline passes.
async fn wait_for_port(
    child: &mut Child,
    output: &Arc<Mutex<String>>,
    timeout: Duration,
) -> Result<u16, String> {
    let deadline = Instant::now() + timeout;

    loop {
        // Scoped so the lock is released before `child` is borrowed.
        let announced = {
            let buffer = output.lock().await;
            ready::parse_ready_port(&buffer)
        };

        if let Some(port) = announced {
            return Ok(port);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "Hermes backend exited before port announcement ({status}).{}",
                    output_tail(output).await
                ))
            }
            Ok(None) => {}
            Err(err) => return Err(format!("Hermes backend wait failed: {err}")),
        }

        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for Hermes backend port announcement ({}ms).{}",
                timeout.as_millis(),
                output_tail(output).await
            ));
        }

        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// Probe `{base}/api/health`, falling back to `{base}/api/status` for backends
/// that predate the health route. Port of `waitForHermesReady`
/// (`electron/backend-health.ts`), minus the remote/OAuth branches the local
/// path never reaches.
pub async fn wait_for_ready(http: &reqwest::Client, base_url: &str, token: &str) -> Result<(), String> {
    let base = base_url.trim_end_matches('/').to_string();
    let deadline = Instant::now() + BACKEND_READY_TIMEOUT;
    let mut use_status_fallback = false;
    let mut last_error = String::from("timeout");

    while Instant::now() < deadline {
        let path = if use_status_fallback { "/api/status" } else { "/api/health" };

        match http
            .get(format!("{base}{path}"))
            .header("X-Hermes-Session-Token", token)
            .timeout(HEALTH_PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status().as_u16();
                last_error = format!("{status}: probe failed");

                // A missing route means the backend predates /api/health.
                if !use_status_fallback && status == 404 {
                    use_status_fallback = true;
                    continue;
                }
            }
            Err(err) => {
                last_error = err.to_string();
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Err(format!("Hermes backend did not become ready: {last_error}"))
}

/// Spawn the backend and wait for it to announce its port.
///
/// Retries once in the legacy `dashboard --no-open` form when the first attempt
/// dies before announcing. A runtime that predates the `serve` subcommand (an
/// older managed install the app has not updated yet, or an older `hermes`
/// resolved from PATH) exits at once on an unknown-argument error, and both
/// forms start the same headless gateway — so retrying on the symptom reaches
/// the same outcome as `electron/backend-command.ts`, which instead inspects the
/// runtime's own `dashboard.py` up front. The retry costs one failed spawn
/// (milliseconds) and avoids having to locate and parse a source tree.
pub async fn spawn_backend(profile: Option<&str>) -> Result<BackendHandle, String> {
    let resolved = command::resolve_backend_command(profile)?;
    let hermes_home = paths::hermes_home();
    let venv_root = paths::venv_root();

    // A fresh session token per backend, handed to the child through the same
    // env var the Electron shell uses and echoed back in the descriptor so
    // renderer requests and the gateway WS both authenticate.
    let token = uuid::Uuid::new_v4().to_string();

    let launched = match start_backend(&resolved.program, &resolved.args, &hermes_home, &venv_root, &token).await
    {
        Ok(launched) => launched,
        Err(first_error) => {
            // Only a `serve` argv has a legacy equivalent worth falling back to.
            if !resolved.args.iter().any(|arg| arg == "serve") {
                return Err(first_error);
            }

            let legacy = command::dashboard_fallback_args(&resolved.args);

            logging::info(&format!(
                "backend rejected the `serve` form ({first_error}); retrying as `dashboard --no-open`"
            ));

            start_backend(&resolved.program, &legacy, &hermes_home, &venv_root, &token)
                .await
                .map_err(|retry_error| {
                    format!("{retry_error}\n--- the `serve` attempt failed with ---\n{first_error}")
                })?
        }
    };

    let (child, pid, port, output) = launched;
    let base_url = format!("http://127.0.0.1:{port}");

    logging::info(&format!("backend ready on {base_url} (pid {pid})"));

    Ok(BackendHandle {
        child,
        pid,
        base_url,
        profile: profile.map(str::to_string),
        token,
        output,
    })
}

/// One spawn attempt: start the child, pump its output, wait for the port
/// announcement. A failed attempt is always reaped before returning, so the
/// fallback retry never leaves the first child holding the venv open.
async fn start_backend(
    program: &Path,
    args: &[String],
    hermes_home: &Path,
    venv_root: &Path,
    token: &str,
) -> Result<(Child, u32, u16, Arc<Mutex<String>>), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.envs(env::build_backend_env(hermes_home, venv_root));
    cmd.env("HERMES_HOME", hermes_home);
    cmd.env("HERMES_DESKTOP", "1");
    cmd.env("HERMES_DASHBOARD_SESSION_TOKEN", token);
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.current_dir(hermes_home);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    #[cfg(windows)]
    {
        // `tokio::process::Command::creation_flags` is an inherent method on
        // Windows, so no `CommandExt` import is needed here. (The `taskkill`
        // path below uses `std::process::Command`, which does need it.)
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    logging::info(&format!("spawning backend: {} {}", program.display(), args.join(" ")));

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Failed to spawn Hermes backend ({}): {err}", program.display()))?;

    let pid = child
        .id()
        .ok_or_else(|| "Hermes backend spawned without a pid".to_string())?;

    let output = Arc::new(Mutex::new(String::new()));

    if let Some(stdout) = child.stdout.take() {
        spawn_output_pump(stdout, Arc::clone(&output), "stdout");
    }

    if let Some(stderr) = child.stderr.take() {
        spawn_output_pump(stderr, Arc::clone(&output), "stderr");
    }

    let timeout = Duration::from_millis(ready::resolve_port_announce_timeout_ms());

    let port = match wait_for_port(&mut child, &output, timeout).await {
        Ok(port) => port,
        Err(err) => {
            // Never leave a half-started backend behind: it would hold the venv
            // open and block the next boot (and the updater).
            stop_child(&mut child, pid).await;

            return Err(err);
        }
    };

    Ok((child, pid, port, output))
}

/// How many lines of retained child output the descriptor carries. The renderer
/// shows them in the backend panel; the buffer itself is capped at
/// `MAX_OUTPUT_BUFFER_BYTES`, which is far too much to ship as JSON.
const DESCRIPTOR_LOG_LINES: usize = 40;

/// The newest `limit` lines of retained child output.
///
/// `try_lock` rather than `lock` because this is a synchronous call that can run
/// while the stdout pump is mid-append; the buffer is diagnostic only, so a
/// missed read degrades to "no logs" instead of stalling the IPC call.
fn output_lines(output: &Arc<Mutex<String>>, limit: usize) -> Vec<String> {
    let Ok(buffer) = output.try_lock() else {
        return Vec::new();
    };

    let lines: Vec<&str> = buffer.lines().collect();
    let start = lines.len().saturating_sub(limit);

    lines[start..].iter().map(|line| (*line).to_string()).collect()
}

/// The profile name a request resolves to.
///
/// An absent profile *is* the default profile — not a distinct backend. Both
/// spellings arrive for the same primary backend: `getConnection()` (the
/// window-owned boot dial) sends nothing, while a routed request sends the
/// active profile's name, which is `"default"` on a fresh install. The Electron
/// shell's routing table agrees — `resolveProfileBackendRoute` sends a falsy
/// scope AND a scope equal to the primary to `primary` — and `command.rs`
/// already folds `None` and `Some("")` into the same argv.
pub fn canonical_profile(profile: Option<&str>) -> &str {
    profile.map(str::trim).filter(|name| !name.is_empty()).unwrap_or("default")
}

/// Whether a live handle for `handle_profile` can serve `requested` without a
/// respawn.
///
/// Comparing the raw `Option`s made the two spellings above look like different
/// backends, so alternating boot calls tore down the healthy child the previous
/// one had just started and cold-spawned a replacement. Five cold starts at
/// ~4.5s overran the renderer's 20s reconnect budget and surfaced as a
/// backend-startup timeout.
pub fn serves_profile(handle_profile: Option<&str>, requested: Option<&str>) -> bool {
    canonical_profile(handle_profile) == canonical_profile(requested)
}

/// Build the `HermesConnection`-shaped descriptor for a live backend.
pub fn descriptor_for(handle: &BackendHandle) -> ConnectionDescriptor {
    let profile = canonical_profile(handle.profile.as_deref()).to_string();

    ConnectionDescriptor {
        base_url: handle.base_url.clone(),
        is_fullscreen: false,
        native_overlay_width: 0.0,
        token: handle.token.clone(),
        ws_url: gateway_ws_url(&handle.base_url, &handle.token),
        logs: output_lines(&handle.output, DESCRIPTOR_LOG_LINES),
        mode: "local".to_string(),
        profile,
        source: "local".to_string(),
        window_button_position: None,
    }
}

/// `ws(s)://<host><prefix>/api/ws?token=<encoded token>`. Ported from
/// `buildGatewayWsUrl` in `electron/connection-config.ts` so the renderer's
/// socket URL matches byte for byte.
pub fn gateway_ws_url(base_url: &str, token: &str) -> String {
    let scheme_secure = base_url.starts_with("https://");
    let scheme = if scheme_secure { "wss" } else { "ws" };

    let without_scheme = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .unwrap_or(base_url);

    let (host, path) = match without_scheme.find('/') {
        Some(index) => (&without_scheme[..index], &without_scheme[index..]),
        None => (without_scheme, ""),
    };

    let prefix = path.trim_end_matches('/');

    format!("{scheme}://{host}{prefix}/api/ws?token={}", encode_uri_component(token))
}

/// `encodeURIComponent` equivalent: RFC 3986 unreserved characters pass
/// through, everything else is percent-encoded.
fn encode_uri_component(value: &str) -> String {
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

/// True when the child is still running.
pub fn is_alive(handle: &mut BackendHandle) -> bool {
    matches!(handle.child.try_wait(), Ok(None))
}

/// Stop a backend and everything it spawned.
///
/// Windows: `taskkill /T /F` — Node's `child.kill()` only signals the direct
/// child, and a backend that spawned grandchildren (a REPL, a pty session, the
/// gateway) survives a plain SIGTERM and keeps files (the venv shim) locked.
/// POSIX: signal the whole process group, since the backend is spawned into its
/// own session and the group send is what reaches MCP grandchildren.
pub async fn stop_child(child: &mut Child, pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }

    #[cfg(not(windows))]
    {
        // `kill` is in POSIX coreutils and always on PATH; the negative pid
        // targets the process group.
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
    }

    let _ = child.kill().await;
}

pub async fn stop_backend(handle: &mut BackendHandle) {
    stop_child(&mut handle.child, handle.pid).await;
    logging::info(&format!("stopped backend pid {}", handle.pid));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_matches_the_electron_contract() {
        assert_eq!(
            gateway_ws_url("http://127.0.0.1:9119", "abc"),
            "ws://127.0.0.1:9119/api/ws?token=abc"
        );
        assert_eq!(
            gateway_ws_url("https://gw.example.com", "tok123"),
            "wss://gw.example.com/api/ws?token=tok123"
        );
        assert_eq!(
            gateway_ws_url("https://host/hermes", "t"),
            "wss://host/hermes/api/ws?token=t"
        );
    }

    #[test]
    fn ws_url_url_encodes_the_token() {
        assert_eq!(
            gateway_ws_url("https://host", "a/b c+d"),
            "wss://host/api/ws?token=a%2Fb%20c%2Bd"
        );
    }

    #[test]
    fn descriptor_carries_a_local_mode_and_token() {
        let handle = BackendHandle {
            child: unreachable_child(),
            pid: 1,
            base_url: "http://127.0.0.1:9119".to_string(),
            profile: None,
            token: "tok".to_string(),
            output: Arc::new(Mutex::new("boot line\n".to_string())),
        };

        let descriptor = descriptor_for(&handle);

        assert_eq!(descriptor.mode, "local");
        assert_eq!(descriptor.profile, "default");
        assert_eq!(descriptor.ws_url, "ws://127.0.0.1:9119/api/ws?token=tok");
        // Retained child output reaches the renderer as a log tail.
        assert_eq!(descriptor.logs, vec!["boot line".to_string()]);
    }

    #[test]
    fn descriptor_logs_keep_only_the_newest_lines() {
        let noise: String = (0..DESCRIPTOR_LOG_LINES + 25)
            .map(|line| format!("line {line}\n"))
            .collect();

        let handle = BackendHandle {
            child: unreachable_child(),
            pid: 1,
            base_url: "http://127.0.0.1:9119".to_string(),
            profile: None,
            token: "tok".to_string(),
            output: Arc::new(Mutex::new(noise)),
        };

        let logs = descriptor_for(&handle).logs;

        assert_eq!(logs.len(), DESCRIPTOR_LOG_LINES);
        assert_eq!(logs[0], format!("line {}", 25));
        assert_eq!(logs[DESCRIPTOR_LOG_LINES - 1], format!("line {}", DESCRIPTOR_LOG_LINES + 24));
    }

    /// The boot path dials the primary backend two ways in the same sequence:
    /// `getConnection()` with no profile, and `getConnection("default")` from the
    /// routed request path. Both name the same backend, so neither may recycle
    /// the other — a recycle costs a full cold start (~4.5s here), and the
    /// renderer budgets the whole dial at 20s.
    #[test]
    fn an_absent_profile_and_the_default_name_share_one_backend() {
        assert!(serves_profile(None, Some("default")));
        assert!(serves_profile(Some("default"), None));
        assert!(serves_profile(Some(""), Some("default")));
        // Whitespace-only reads as unset, matching `ready::resolved_profile`.
        assert!(serves_profile(Some("  "), None));

        // A named profile is a genuinely different backend — the shell spawns it
        // with `--profile` — so it must still be recycled.
        assert!(!serves_profile(Some("work"), None));
        assert!(!serves_profile(None, Some("work")));
        assert!(!serves_profile(Some("work"), Some("default")));
    }

    /// A `Child` value that is never polled or killed — the descriptor test
    /// only reads scalar fields off the handle.
    fn unreachable_child() -> Child {
        Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .arg(if cfg!(windows) { "/C" } else { "" })
            .arg(if cfg!(windows) { "exit" } else { "" })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn placeholder child")
    }
}
