//! First-launch bootstrap: install a Hermes runtime by driving the platform
//! installer (`scripts/install.ps1` / `scripts/install.sh`) stage by stage.
//!
//! A faithful port of `electron/bootstrap-runner.ts` (plus the pieces of
//! `bootstrap-platform.ts` it calls into), with the same contract: resolve the
//! installer, fetch its manifest, run each stage with `-NonInteractive -Json`,
//! parse the JSON result frames, and write the bootstrap-complete marker. The
//! pure decisions live as standalone functions so they are unit-tested the same
//! way the Electron tests (`bootstrap-runner.test.ts`) pinned them; the spawn
//! orchestration is thin over `tokio::process`.
//!
//! ## Event contract
//!
//! The runner emits `BootstrapEvent` values; the command layer broadcasts them
//! to the renderer as `hermes:bootstrap-event`. The shapes are the renderer's
//! `DesktopBootstrapEvent` union (`src/global.d.ts`), field-for-field — the
//! install overlay merges them with `getBootstrapState()` and renders progress.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// The build-time install stamp (`install-stamp.json`), or `None` in dev.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallStamp {
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

/// `DesktopBootstrapStageDescriptor` from `src/global.d.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageDescriptor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub needs_user_input: Option<bool>,
}

/// Resolve the build-time install stamp: an explicit
/// `HERMES_DESKTOP_INSTALL_STAMP` (a JSON object or a path to one) wins,
/// otherwise a bundled `install-stamp.json` in the app's resource directory
/// (checked both flat and under `build/`, since Tauri's resource placement
/// differs across versions), then next to the executable.
pub fn resolve_install_stamp(resource_dir: Option<&Path>) -> Option<InstallStamp> {
    if let Ok(raw) = std::env::var("HERMES_DESKTOP_INSTALL_STAMP") {
        let trimmed = raw.trim();

        if !trimmed.is_empty() {
            if let Ok(stamp) = serde_json::from_str::<InstallStamp>(trimmed) {
                return Some(stamp);
            }

            if let Ok(text) = std::fs::read_to_string(trimmed) {
                if let Ok(stamp) = serde_json::from_str::<InstallStamp>(&text) {
                    return Some(stamp);
                }
            }
        }
    }

    if let Some(dir) = resource_dir {
        for candidate in [
            dir.join("install-stamp.json"),
            dir.join("build").join("install-stamp.json"),
        ] {
            if let Some(stamp) = read_install_stamp_file(&candidate) {
                return Some(stamp);
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(stamp) = read_install_stamp_file(&dir.join("install-stamp.json")) {
                return Some(stamp);
            }
        }
    }

    None
}

fn read_install_stamp_file(path: &Path) -> Option<InstallStamp> {
    let text = std::fs::read_to_string(path).ok()?;

    serde_json::from_str::<InstallStamp>(&text).ok()
}

/// The managed checkout root, mirroring Electron's `ACTIVE_HERMES_ROOT`.
pub fn active_root(hermes_home: &Path) -> PathBuf {
    hermes_home.join("hermes-agent")
}

/// The install-script manifest, as produced by `install.ps1 -Manifest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestPayload {
    pub stages: Vec<StageDescriptor>,
    pub protocol_version: Option<serde_json::Value>,
}

/// One JSON result frame from `install.ps1 -Stage <n> -Json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StageResult {
    pub ok: bool,
    #[serde(default)]
    pub skipped: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub stage: String,
}

/// `DesktopBootstrapEvent` from `src/global.d.ts`, minus the decision-layer
/// events (`setup-choice`, `dismissed`, `unsupported-platform`) which the
/// command layer emits, not the runner.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BootstrapEvent {
    #[serde(rename_all = "camelCase")]
    Manifest {
        stages: Vec<StageDescriptor>,
        protocol_version: Option<serde_json::Value>,
    },
    #[serde(rename_all = "camelCase")]
    Stage {
        name: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        json: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Log {
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        line: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        stream: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Complete { marker: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    Failed {
        #[serde(skip_serializing_if = "Option::is_none")]
        stage: Option<String>,
        error: String,
    },
}

/// The terminal outcome of a bootstrap run, mirroring `runBootstrap`'s return.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapOutcome {
    pub ok: bool,
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ─── Pure decisions ──────────────────────────────────────────────────────────

/// A real git commit pin: 7..=40 hex chars, and not the all-zero placeholder
/// that non-git builds stamp.
pub fn is_pinned_commit(commit: Option<&str>) -> bool {
    let Some(commit) = commit else {
        return false;
    };

    let bytes = commit.as_bytes();

    (7..=40).contains(&bytes.len())
        && bytes.iter().all(|byte| byte.is_ascii_hexdigit())
        && !bytes.iter().all(|byte| *byte == b'0')
}

fn is_fallback_commit(commit: Option<&str>) -> bool {
    commit
        .map(|value| {
            let bytes = value.as_bytes();
            !bytes.is_empty() && bytes.iter().all(|byte| *byte == b'0')
        })
        .unwrap_or(false)
}

/// The ref to fetch `install.ps1`/`install.sh` from, plus its cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRef {
    pub ref_name: String,
    pub cache_key: String,
    pub pinned: bool,
}

/// Map an install stamp to the GitHub ref used to fetch the installer. Real
/// pins are immutable SHAs; all-zero fallback stamps become an unpinned branch
/// ref so bootstrap never asks GitHub for commit `0000000…` (#50823).
pub fn install_ref_for_stamp(stamp: Option<&InstallStamp>) -> Option<InstallRef> {
    let stamp = stamp?;
    let commit = stamp.commit.as_deref();

    if is_pinned_commit(commit) {
        let ref_name = commit.unwrap().to_string();

        return Some(InstallRef {
            ref_name: ref_name.clone(),
            cache_key: ref_name,
            pinned: true,
        });
    }

    if is_fallback_commit(commit) {
        let branch = stamp.branch.as_deref().unwrap_or("main");
        let cache_key = format!("fallback-{}", branch.replace(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '_' && c != '-', "_"));

        return Some(InstallRef {
            ref_name: branch.to_string(),
            cache_key,
            pinned: false,
        });
    }

    None
}

/// The installer branch/pin args. Fresh installs pin the commit; once a managed
/// checkout already exists, bootstrap is a repair path and must not detach the
/// checkout back to the commit baked into an old app (#50823 / #50864).
pub fn build_pin_args(stamp: Option<&InstallStamp>, pin_commit: bool) -> Vec<String> {
    let mut args = Vec::new();

    if pin_commit {
        if let Some(commit) = stamp.and_then(|s| s.commit.as_deref()) {
            if is_pinned_commit(Some(commit)) {
                args.push("-Commit".to_string());
                args.push(commit.to_string());
            }
        }
    }

    if let Some(branch) = stamp.and_then(|s| s.branch.as_deref()) {
        args.push("-Branch".to_string());
        args.push(branch.to_string());
    }

    args
}

/// Pick the commit stored on the bootstrap-complete marker. Packaged fallback
/// (all-zero) stamps must not win; after a successful install, the checkout's
/// HEAD (or the installer's own marker) does.
pub fn resolve_marker_pinned_commit(
    stamp: Option<&InstallStamp>,
    head: Option<&str>,
    existing_pinned: Option<&str>,
) -> Option<String> {
    if let Some(commit) = stamp.and_then(|s| s.commit.as_deref()) {
        if is_pinned_commit(Some(commit)) {
            return Some(commit.to_string());
        }
    }

    if let Some(head) = head.filter(|value| is_pinned_commit(Some(value))) {
        return Some(head.to_string());
    }

    existing_pinned
        .filter(|value| is_pinned_commit(Some(value)))
        .map(str::to_string)
}

/// Parse the JSON result frame from a stage run: the last line that carries
/// `{ ok: bool, stage: string }`. The protocol guarantees exactly one such line.
pub fn parse_stage_result(stdout: &str) -> Option<StageResult> {
    for line in stdout.lines().rev() {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        if let Ok(parsed) = serde_json::from_str::<StageResult>(line) {
            return Some(parsed);
        }
    }

    None
}

/// Parse the manifest: the last line of stdout that parses as JSON with a
/// `stages` array (install.ps1 may print banner lines first).
pub fn parse_manifest_payload(stdout: &str) -> Option<ManifestPayload> {
    for line in stdout.lines().rev() {
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        if let Ok(parsed) = serde_json::from_str::<ManifestPayload>(line) {
            return Some(parsed);
        }
    }

    None
}

// ─── Installer resolution ────────────────────────────────────────────────────

fn install_script_name(is_windows: bool) -> &'static str {
    if is_windows {
        "install.ps1"
    } else {
        "install.sh"
    }
}

fn install_script_ext(is_windows: bool) -> &'static str {
    if is_windows {
        "ps1"
    } else {
        "sh"
    }
}

fn bootstrap_cache_dir(hermes_home: &Path) -> PathBuf {
    hermes_home.join("bootstrap-cache")
}

fn cached_script_path(hermes_home: &Path, cache_key: &str, is_windows: bool) -> PathBuf {
    bootstrap_cache_dir(hermes_home).join(format!("install-{cache_key}.{}", install_script_ext(is_windows)))
}

fn resolve_local_install_script(source_repo_root: Option<&Path>, is_windows: bool) -> Option<PathBuf> {
    let root = source_repo_root?;
    let candidate = root.join("scripts").join(install_script_name(is_windows));

    candidate.is_file().then_some(candidate)
}

/// The installer that ships inside an already-installed agent checkout, used as
/// a last-resort fallback when the pinned ref cannot be fetched from GitHub.
fn installed_agent_install_script(hermes_home: &Path, is_windows: bool) -> Option<PathBuf> {
    let candidate = hermes_home
        .join("hermes-agent")
        .join("scripts")
        .join(install_script_name(is_windows));

    candidate.is_file().then_some(candidate)
}

/// Download `install.ps1`/`install.sh` from GitHub raw at `ref`, following one
/// redirect defensively (GitHub raw does not redirect for a SHA URL).
async fn download_install_script(http: &reqwest::Client, ref_name: &str, dest: &Path, is_windows: bool) -> Result<(), String> {
    let name = install_script_name(is_windows);
    let url = format!("https://raw.githubusercontent.com/NousResearch/hermes-agent/{ref_name}/scripts/{name}");

    let bytes = http
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("Failed to download {name}: {err}"))?
        .error_for_status()
        .map_err(|err| format!("Failed to download {name}: {err}"))?
        .bytes()
        .await
        .map_err(|err| format!("Failed to read {name}: {err}"))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("Failed to create {}: {err}", parent.display()))?;
    }

    std::fs::write(dest, bytes).map_err(|err| format!("Failed to write {}: {err}", dest.display()))
}

/// The result of resolving the installer script.
struct ResolvedScript {
    path: PathBuf,
}

/// Resolve the installer: dev local checkout → cached → GitHub download →
/// installed-agent fallback. Ported verbatim from `resolveInstallScript`.
async fn resolve_install_script(
    http: &reqwest::Client,
    stamp: Option<&InstallStamp>,
    source_repo_root: Option<&Path>,
    hermes_home: &Path,
    is_windows: bool,
    on_event: &mut (dyn FnMut(BootstrapEvent) + Send),
) -> Result<ResolvedScript, String> {
    let name = install_script_name(is_windows);

    if let Some(local) = resolve_local_install_script(source_repo_root, is_windows) {
        on_event(BootstrapEvent::Log {
            stage: None,
            line: format!("[bootstrap] using local {name} at {}", local.display()),
            stream: Some("stdout".to_string()),
        });

        return Ok(ResolvedScript { path: local });
    }

    let install_ref = install_ref_for_stamp(stamp).ok_or_else(|| {
        format!("Cannot resolve {name}: no source checkout and no install stamp. This packaged build was produced without a valid build-time stamp.")
    })?;

    let cached = cached_script_path(hermes_home, &install_ref.cache_key, is_windows);
    let short_ref = &install_ref.ref_name[..install_ref.ref_name.len().min(12)];
    let unpinned = if install_ref.pinned { "" } else { " (fallback, unpinned)" };

    if cached.is_file() {
        on_event(BootstrapEvent::Log {
            stage: None,
            line: format!("[bootstrap] using cached {name} for {short_ref}{unpinned}"),
            stream: Some("stdout".to_string()),
        });

        return Ok(ResolvedScript { path: cached });
    }

    on_event(BootstrapEvent::Log {
        stage: None,
        line: format!("[bootstrap] fetching {name} for {short_ref} from GitHub{unpinned}"),
        stream: Some("stdout".to_string()),
    });

    if let Err(download_error) = download_install_script(http, &install_ref.ref_name, &cached, is_windows).await {
        let installed = installed_agent_install_script(hermes_home, is_windows);

        if let Some(installed) = installed {
            on_event(BootstrapEvent::Log {
                stage: None,
                line: format!(
                    "[bootstrap] GitHub fetch failed ({download_error}); falling back to installed agent {name} at {}",
                    installed.display()
                ),
                stream: Some("stdout".to_string()),
            });

            return Ok(ResolvedScript { path: installed });
        }

        return Err(download_error);
    }

    on_event(BootstrapEvent::Log {
        stage: None,
        line: format!("[bootstrap] saved to {}", cached.display()),
        stream: Some("stdout".to_string()),
    });

    Ok(ResolvedScript { path: cached })
}

// ─── Process orchestration ───────────────────────────────────────────────────

/// Spawn the installer with the given args, streaming stdout/stderr to the log.
async fn spawn_installer(
    script: &Path,
    args: &[String],
    hermes_home: &Path,
    is_windows: bool,
    stage_name: &str,
    on_event: &mut (dyn FnMut(BootstrapEvent) + Send),
) -> Result<(String, String, Option<i32>), String> {
    let program = if is_windows { "powershell.exe" } else { "bash" };

    let mut cmd = Command::new(program);

    if is_windows {
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
    }

    cmd.arg(script);
    cmd.args(args);
    cmd.env("HERMES_HOME", hermes_home);
    cmd.current_dir(hermes_home);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("Failed to spawn {program} ({}): {err}", script.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Read both pipes to EOF *concurrently* before waiting, so a stage that
    // fills one pipe cannot deadlock the child while the other is drained.
    async fn drain<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>) -> String {
        let Some(mut stream) = stream else {
            return String::new();
        };

        let mut text = String::new();
        let mut buf = [0u8; 8192];

        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }

        text
    }

    let (stdout_text, stderr_text) = tokio::join!(drain(stdout), drain(stderr));

    let status = child
        .wait()
        .await
        .map_err(|err| format!("Failed to wait for installer: {err}"))?;

    // Stream line-by-line so the renderer sees progress in real time, exactly
    // like the TS runner.
    for line in stdout_text.lines() {
        if !line.is_empty() {
            on_event(BootstrapEvent::Log {
                stage: Some(stage_name.to_string()),
                line: line.to_string(),
                stream: Some("stdout".to_string()),
            });
        }
    }

    for line in stderr_text.lines() {
        if !line.is_empty() {
            on_event(BootstrapEvent::Log {
                stage: Some(stage_name.to_string()),
                line: line.to_string(),
                stream: Some("stderr".to_string()),
            });
        }
    }

    Ok((stdout_text, stderr_text, status.code()))
}

fn posix_pin_args(stamp: Option<&InstallStamp>, active_root: &Path, hermes_home: &Path, pin_commit: bool) -> Vec<String> {
    let mut args = vec![
        "--dir".to_string(),
        active_root.display().to_string(),
        "--hermes-home".to_string(),
        hermes_home.display().to_string(),
    ];

    if let Some(branch) = stamp.and_then(|s| s.branch.as_deref()) {
        args.push("--branch".to_string());
        args.push(branch.to_string());
    }

    if pin_commit {
        if let Some(commit) = stamp.and_then(|s| s.commit.as_deref()) {
            if is_pinned_commit(Some(commit)) {
                args.push("--commit".to_string());
                args.push(commit.to_string());
            }
        }
    }

    args
}

/// Fetch the manifest (`install.ps1 -Manifest`).
async fn fetch_manifest(
    script: &Path,
    stamp: Option<&InstallStamp>,
    active_root: &Path,
    hermes_home: &Path,
    is_windows: bool,
    pin_commit: bool,
    on_event: &mut (dyn FnMut(BootstrapEvent) + Send),
) -> Result<ManifestPayload, String> {
    let args = if is_windows {
        let mut args = vec!["-Manifest".to_string()];
        args.extend(build_pin_args(stamp, pin_commit));

        args
    } else {
        let mut args = vec!["--manifest".to_string()];
        args.extend(posix_pin_args(stamp, active_root, hermes_home, pin_commit));

        args
    };

    let (stdout, stderr, code) = spawn_installer(script, &args, hermes_home, is_windows, "__manifest__", on_event).await?;

    if code != Some(0) {
        return Err(format!("installer --manifest failed: exit {code:?}\n{}", if stderr.is_empty() { &stdout } else { &stderr }));
    }

    parse_manifest_payload(&stdout).ok_or_else(|| format!("installer --manifest produced no parseable JSON payload\n{stdout}"))
}

/// Run one stage (`install.ps1 -Stage <name> -NonInteractive -Json`).
async fn run_stage(
    script: &Path,
    stamp: Option<&InstallStamp>,
    stage: &StageDescriptor,
    active_root: &Path,
    hermes_home: &Path,
    is_windows: bool,
    pin_commit: bool,
    on_event: &mut (dyn FnMut(BootstrapEvent) + Send),
) -> BootstrapEvent {
    let started = std::time::Instant::now();

    on_event(BootstrapEvent::Stage {
        name: stage.name.clone(),
        state: "running".to_string(),
        duration_ms: None,
        json: None,
        error: None,
    });

    let args = if is_windows {
        let mut args = vec![
            "-Stage".to_string(),
            stage.name.clone(),
            "-NonInteractive".to_string(),
            "-Json".to_string(),
        ];
        args.extend(build_pin_args(stamp, pin_commit));

        args
    } else {
        let mut args = vec![
            "--stage".to_string(),
            stage.name.clone(),
            "--non-interactive".to_string(),
            "--json".to_string(),
        ];
        args.extend(posix_pin_args(stamp, active_root, hermes_home, pin_commit));

        args
    };

    let duration = || started.elapsed().as_millis() as u64;

    let result = spawn_installer(script, &args, hermes_home, is_windows, &stage.name, on_event).await;

    let (stdout, _stderr, _code) = match result {
        Ok(output) => output,
        Err(error) => {
            return BootstrapEvent::Stage {
                name: stage.name.clone(),
                state: "failed".to_string(),
                duration_ms: Some(duration()),
                json: None,
                error: Some(error),
            };
        }
    };

    let json = parse_stage_result(&stdout);

    let Some(json) = json else {
        return BootstrapEvent::Stage {
            name: stage.name.clone(),
            state: "failed".to_string(),
            duration_ms: Some(duration()),
            json: None,
            error: Some(format!("installer --stage {} produced no JSON result frame", stage.name)),
        };
    };

    let state = if json.ok && json.skipped.unwrap_or(false) {
        "skipped"
    } else if json.ok {
        "succeeded"
    } else {
        "failed"
    };

    let error = if state == "failed" {
        Some(json.reason.clone().unwrap_or_else(|| "unknown error".to_string()))
    } else {
        None
    };

    BootstrapEvent::Stage {
        name: stage.name.clone(),
        state: state.to_string(),
        duration_ms: Some(duration()),
        json: Some(serde_json::to_value(&json).unwrap_or(serde_json::Value::Null)),
        error,
    }
}

/// The full bootstrap orchestration, mirroring `runBootstrap`.
pub async fn run_bootstrap(
    http: &reqwest::Client,
    stamp: Option<&InstallStamp>,
    active_root: &Path,
    source_repo_root: Option<&Path>,
    hermes_home: &Path,
    is_windows: bool,
    cancel: &std::sync::atomic::AtomicBool,
    on_event: &mut (dyn FnMut(BootstrapEvent) + Send),
) -> BootstrapOutcome {
    let cancelled = || cancel.load(std::sync::atomic::Ordering::SeqCst);

    if cancelled() {
        on_event(BootstrapEvent::Failed { stage: None, error: "bootstrap cancelled by user".to_string() });

        return BootstrapOutcome { ok: false, cancelled: true, failed_stage: None, error: None };
    }

    let existing_checkout = active_root.join(".git").exists();
    let pin_commit = !existing_checkout;

    // 1. Resolve the installer.
    let script = match resolve_install_script(http, stamp, source_repo_root, hermes_home, is_windows, on_event).await {
        Ok(script) => script,
        Err(error) => {
            on_event(BootstrapEvent::Failed { stage: None, error: error.clone() });

            return BootstrapOutcome { ok: false, cancelled: false, failed_stage: None, error: Some(error) };
        }
    };

    // 2. Fetch the manifest.
    let manifest = match fetch_manifest(&script.path, stamp, active_root, hermes_home, is_windows, pin_commit, on_event).await {
        Ok(manifest) => manifest,
        Err(error) => {
            on_event(BootstrapEvent::Failed { stage: None, error: error.clone() });

            return BootstrapOutcome { ok: false, cancelled: false, failed_stage: None, error: Some(error) };
        }
    };

    on_event(BootstrapEvent::Manifest {
        stages: manifest.stages.clone(),
        protocol_version: manifest.protocol_version.clone(),
    });

    // 3. Run each stage in order.
    for stage in &manifest.stages {
        if cancelled() {
            on_event(BootstrapEvent::Failed { stage: None, error: "bootstrap cancelled by user".to_string() });

            return BootstrapOutcome { ok: false, cancelled: true, failed_stage: None, error: None };
        }

        let event = run_stage(&script.path, stamp, stage, active_root, hermes_home, is_windows, pin_commit, on_event).await;

        on_event(event.clone());

        if let BootstrapEvent::Stage { state, error, .. } = event {
            if state == "failed" {
                let error = error.unwrap_or_else(|| "stage failed".to_string());

                on_event(BootstrapEvent::Failed {
                    stage: Some(stage.name.clone()),
                    error: error.clone(),
                });

                return BootstrapOutcome {
                    ok: false,
                    cancelled: false,
                    failed_stage: Some(stage.name.clone()),
                    error: Some(error),
                };
            }
        }
    }

    // 4. Resolve the marker's pinned commit and write it. All-zero fallback
    // stamps are not real pins — resolve HEAD from the fresh checkout instead.
    let head = resolve_checkout_head(active_root).await;
    let existing = read_existing_pinned_commit(active_root);
    let pinned_commit = resolve_marker_pinned_commit(stamp, head.as_deref(), existing.as_deref());

    let marker = serde_json::json!({
        "pinnedCommit": pinned_commit,
        "pinnedBranch": stamp.and_then(|s| s.branch.clone()),
    });

    if let Err(error) = write_bootstrap_marker(active_root, &marker) {
        on_event(BootstrapEvent::Failed { stage: None, error: error.clone() });

        return BootstrapOutcome { ok: false, cancelled: false, failed_stage: None, error: Some(error) };
    }

    on_event(BootstrapEvent::Complete { marker });

    BootstrapOutcome { ok: true, cancelled: false, failed_stage: None, error: None }
}

// ─── Marker / checkout helpers ───────────────────────────────────────────────

/// `git rev-parse HEAD` from a managed checkout, or `None` when it is not a
/// real pinned commit.
pub async fn resolve_checkout_head(active_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-c", "windows.appendAtomically=false", "rev-parse", "HEAD"])
        .current_dir(active_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();

    is_pinned_commit(Some(&sha)).then_some(sha)
}

/// A real pin already written by the installer's bootstrap-marker stage.
pub fn read_existing_pinned_commit(active_root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(active_root.join(".hermes-bootstrap-complete")).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;

    parsed
        .get("pinnedCommit")
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_pinned_commit(Some(value)))
        .map(str::to_string)
}

/// Write the `.hermes-bootstrap-complete` marker, creating the root first.
pub fn write_bootstrap_marker(active_root: &Path, marker: &serde_json::Value) -> Result<(), String> {
    std::fs::create_dir_all(active_root).map_err(|err| format!("Failed to create {}: {err}", active_root.display()))?;

    let text = serde_json::to_string_pretty(marker).map_err(|err| format!("Failed to encode marker: {err}"))?;

    std::fs::write(active_root.join(".hermes-bootstrap-complete"), text)
        .map_err(|err| format!("Failed to write marker: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(commit: Option<&str>, branch: Option<&str>) -> InstallStamp {
        InstallStamp {
            commit: commit.map(str::to_string),
            branch: branch.map(str::to_string),
        }
    }

    #[test]
    fn pin_validation_matches_electron() {
        assert!(is_pinned_commit(Some("a1b2c3d")));
        assert!(is_pinned_commit(Some("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d")));
        assert!(!is_pinned_commit(Some("0000000")));
        assert!(!is_pinned_commit(Some("abc")));
        assert!(!is_pinned_commit(None));
        assert!(!is_pinned_commit(Some("not-a-sha!")));
    }

    #[test]
    fn install_ref_prefers_a_real_pin_and_never_asks_github_for_zeros() {
        let pinned = install_ref_for_stamp(Some(&stamp(Some("a1b2c3d4e5f"), Some("main")))).unwrap();

        assert!(pinned.pinned);
        assert_eq!(pinned.ref_name, "a1b2c3d4e5f");
        assert_eq!(pinned.cache_key, "a1b2c3d4e5f");

        let fallback = install_ref_for_stamp(Some(&stamp(Some("0000000000000000000000000000000000000000"), Some("dev")))).unwrap();

        assert!(!fallback.pinned);
        assert_eq!(fallback.ref_name, "dev");
        assert!(fallback.cache_key.starts_with("fallback-"));

        assert!(install_ref_for_stamp(None).is_none());
        assert!(install_ref_for_stamp(Some(&stamp(None, None))).is_none());
    }

    #[test]
    fn pin_args_are_commit_then_branch_and_only_when_pinned() {
        let pinned = build_pin_args(Some(&stamp(Some("a1b2c3d4e5f"), Some("main"))), true);

        assert_eq!(pinned, vec!["-Commit", "a1b2c3d4e5f", "-Branch", "main"]);

        // A repair on an existing checkout must not pin.
        let repair = build_pin_args(Some(&stamp(Some("a1b2c3d4e5f"), Some("main"))), false);

        assert_eq!(repair, vec!["-Branch", "main"]);

        // All-zero fallback never becomes -Commit.
        let fallback = build_pin_args(Some(&stamp(Some("00000000000"), None)), true);

        assert_eq!(fallback, Vec::<String>::new());
    }

    #[test]
    fn marker_pin_resolution_prefers_stamp_then_head_then_existing() {
        assert_eq!(
            resolve_marker_pinned_commit(Some(&stamp(Some("a1b2c3d4e5f"), None)), None, None).as_deref(),
            Some("a1b2c3d4e5f")
        );

        // Fallback stamp: HEAD wins over the existing marker.
        assert_eq!(
            resolve_marker_pinned_commit(
                Some(&stamp(Some("00000000000"), None)),
                Some("deadbeefcafe"),
                Some("a1b2c3d4e5f"),
            )
            .as_deref(),
            Some("deadbeefcafe")
        );

        // No stamp, no HEAD: the existing real pin survives.
        assert_eq!(
            resolve_marker_pinned_commit(None, None, Some("a1b2c3d4e5f")).as_deref(),
            Some("a1b2c3d4e5f")
        );

        assert!(resolve_marker_pinned_commit(None, None, None).is_none());
    }

    #[test]
    fn stage_result_parses_the_last_json_frame() {
        let stdout = "banner line\n{\"ok\":true,\"stage\":\"python\"}\n";

        let parsed = parse_stage_result(stdout).unwrap();

        assert!(parsed.ok);
        assert_eq!(parsed.stage, "python");

        assert!(parse_stage_result("no json here").is_none());
    }

    #[test]
    fn manifest_parses_the_last_stages_payload() {
        let stdout = "info\n{\"stages\":[{\"name\":\"python\"},{\"name\":\"venv\"}],\"protocolVersion\":1}\n";

        let parsed = parse_manifest_payload(stdout).unwrap();

        assert_eq!(parsed.stages.len(), 2);
        assert_eq!(parsed.stages[0].name, "python");
        assert_eq!(parsed.stages[1].name, "venv");
    }

    /// The real `install-stamp.json` (from `write-build-stamp.mjs`) carries
    /// `schemaVersion` / `builtAt` / `dirty` / `source` beyond `commit` and
    /// `branch`. `InstallStamp` must ignore those — a `deny_unknown_fields`
    /// slip would make a packaged build fail to read its own stamp and, with
    /// it, the first-launch bootstrap.
    #[test]
    fn install_stamp_ignores_the_extra_schema_fields() {
        let dir = std::env::temp_dir().join(format!("hermes-stamp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("install-stamp.json");

        std::fs::write(
            &path,
            r#"{
                "schemaVersion": 1,
                "commit": "a1b2c3d4e5f6a7b8",
                "branch": "main",
                "builtAt": "2026-01-01T00:00:00Z",
                "dirty": false,
                "source": "ci"
            }"#,
        )
        .unwrap();

        let stamp = read_install_stamp_file(&path).unwrap();

        assert_eq!(stamp.commit.as_deref(), Some("a1b2c3d4e5f6a7b8"));
        assert_eq!(stamp.branch.as_deref(), Some("main"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
