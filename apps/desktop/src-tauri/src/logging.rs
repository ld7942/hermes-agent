//! Desktop shell logging.
//!
//! The Electron main process streams to `desktop.log` under `HERMES_HOME/logs/`
//! and the renderer's error boundary posts to the same file. Keep that contract:
//! `hermes logs` and the in-app log viewer both read this path.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Serializes appends so concurrent command handlers cannot interleave a line.
static WRITE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();

    // Milliseconds since the epoch, matching the Electron shell's line prefix
    // closely enough for `hermes logs` tailing.
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

/// Append one line to `desktop.log`. Never panics: a shell that cannot write
/// its own log must still run.
pub fn append(line: &str) {
    let path = crate::paths::desktop_log_path();

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let _guard = WRITE_LOCK.lock();

    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{}] {}", timestamp(), line);
    }
}

pub fn info(line: &str) {
    append(&format!("INFO  {}", line));
}

pub fn warn(line: &str) {
    append(&format!("WARN  {}", line));
}

pub fn error(line: &str) {
    append(&format!("ERROR {}", line));
}

/// The tail the `hermes:logs:recent` handler returns to the renderer.
pub fn recent(max_bytes: usize) -> String {
    let path = crate::paths::desktop_log_path();

    let Ok(contents) = std::fs::read_to_string(&path) else {
        return String::new();
    };

    if contents.len() <= max_bytes {
        return contents;
    }

    // Cut on a char boundary so a multi-byte UTF-8 sequence never splits.
    let start = contents.len() - max_bytes;
    let start = contents
        .char_indices()
        .find(|(idx, _)| *idx >= start)
        .map(|(idx, _)| idx)
        .unwrap_or(0);

    contents[start..].to_string()
}
