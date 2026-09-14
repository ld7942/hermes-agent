//! Resolving the desktop-managed Hermes backend invocation.
//!
//! The shell launches its own headless backend via `hermes serve` — it must
//! NEVER depend on the browser `dashboard`. Mirrors
//! `electron/backend-command.ts`, including the `serve` → `dashboard --no-open`
//! fallback for managed installs that predate the `serve` subcommand (both
//! produce the same headless gateway; `serve` is just the decoupled name).

use std::path::{Path, PathBuf};

use crate::paths;

#[derive(Debug, Clone)]
pub struct BackendCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The canonical headless backend argv (always `serve`).
pub fn serve_backend_args(profile: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = Vec::with_capacity(8);

    if let Some(name) = profile.filter(|name| !name.is_empty()) {
        args.push("--profile".to_string());
        args.push(name.to_string());
    }

    args.extend(
        ["serve", "--host", "127.0.0.1", "--port", "0"]
            .iter()
            .map(|part| part.to_string()),
    );

    args
}

/// Rewrite a resolved argv from `serve` to the legacy `dashboard --no-open`
/// form, preserving every other argument (including a leading
/// `-m hermes_cli.main` and any `--profile <name>`). If there is no `serve`
/// token the argv is returned unchanged.
pub fn dashboard_fallback_args(args: &[String]) -> Vec<String> {
    let Some(index) = args.iter().position(|arg| arg == "serve") else {
        return args.to_vec();
    };

    let mut next = Vec::with_capacity(args.len() + 1);
    next.extend_from_slice(&args[..index]);
    next.push("dashboard".to_string());
    next.push("--no-open".to_string());
    next.extend_from_slice(&args[index + 1..]);

    next
}

/// `<venv>/Scripts/hermes.exe` on Windows, `<venv>/bin/hermes` on POSIX.
fn venv_candidates() -> Vec<PathBuf> {
    let bin = paths::venv_bin_dir();

    if cfg!(windows) {
        vec![bin.join("hermes.exe"), bin.join("hermes")]
    } else {
        vec![bin.join("hermes")]
    }
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Minimal `which`: PATH lookup with the platform executable suffix.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }

        let direct = dir.join(name);

        if is_executable_file(&direct) {
            return Some(direct);
        }

        if cfg!(windows) {
            let with_suffix = dir.join(format!("{name}.exe"));

            if is_executable_file(&with_suffix) {
                return Some(with_suffix);
            }
        }
    }

    None
}

/// Resolve the backend executable, preferring the managed venv over PATH so a
/// stale global `hermes` cannot shadow the install the app manages.
pub fn resolve_backend_command(profile: Option<&str>) -> Result<BackendCommand, String> {
    // An explicit override wins — this is the escape hatch for developers
    // running the shell against an arbitrary checkout.
    if let Some(raw) = std::env::var_os("HERMES_DESKTOP_BACKEND") {
        let value = raw.to_string_lossy().trim().to_string();

        if !value.is_empty() {
            return Ok(BackendCommand {
                program: PathBuf::from(value),
                args: serve_backend_args(profile),
            });
        }
    }

    for candidate in venv_candidates() {
        if is_executable_file(&candidate) {
            return Ok(BackendCommand {
                program: candidate,
                args: serve_backend_args(profile),
            });
        }
    }

    if let Some(found) = which("hermes") {
        return Ok(BackendCommand {
            program: found,
            args: serve_backend_args(profile),
        });
    }

    Err(format!(
        "Hermes backend not found. Looked for a managed runtime at {} and for `hermes` on PATH.",
        paths::venv_bin_dir().display()
    ))
}

/// Whether a spawn failure is specifically "no backend installed" — the one
/// failure the boot path may recover from by running the first-launch installer.
/// Kept as a message test rather than a typed error so the string the renderer
/// already shows is the single source of truth.
pub fn is_backend_not_found(error: &str) -> bool {
    error.starts_with("Hermes backend not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_args_without_profile() {
        assert_eq!(
            serve_backend_args(None),
            vec!["serve", "--host", "127.0.0.1", "--port", "0"]
        );
    }

    #[test]
    fn serve_args_lead_with_profile() {
        assert_eq!(
            serve_backend_args(Some("work")),
            vec!["--profile", "work", "serve", "--host", "127.0.0.1", "--port", "0"]
        );
    }

    #[test]
    fn empty_profile_is_ignored() {
        assert_eq!(serve_backend_args(Some("")), serve_backend_args(None));
    }

    #[test]
    fn dashboard_fallback_rewrites_only_the_subcommand() {
        let args: Vec<String> = ["--profile", "work", "serve", "--host", "127.0.0.1", "--port", "0"]
            .iter()
            .map(|part| part.to_string())
            .collect();

        assert_eq!(
            dashboard_fallback_args(&args),
            vec!["--profile", "work", "dashboard", "--no-open", "--host", "127.0.0.1", "--port", "0"]
        );
    }

    #[test]
    fn dashboard_fallback_is_a_noop_without_serve() {
        let args: Vec<String> = ["dashboard", "--no-open"].iter().map(|p| p.to_string()).collect();
        assert_eq!(dashboard_fallback_args(&args), args);
    }
}
