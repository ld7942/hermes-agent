//! Shared runtime state managed by Tauri.
//!
//! One `AppState` for the whole process: the primary backend slot, the terminal
//! registry, and the HTTP client the `hermes:api` proxy reuses (a fresh client
//! per request would throw away the connection pool and defeat keep-alive).

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::backend::BackendHandle;
use crate::boot::{BootProgress, BootstrapState};
use crate::terminal::TerminalRegistry;

pub struct AppState {
    /// Shared HTTP client for the backend proxy.
    pub http: reqwest::Client,
    /// The primary backend. `None` until the renderer asks for a connection.
    pub backend: Mutex<Option<BackendHandle>>,
    /// Where the spawn has got to, for the renderer's boot overlay. Written on
    /// every backend transition, read by `getBootProgress`.
    pub boot: BootProgress,
    /// The first-launch installer state, fed by the bootstrap runner and read by
    /// `getBootstrapState` / `onBootstrapEvent`.
    pub bootstrap: BootstrapState,
    /// Live PTY sessions, keyed by session id.
    pub terminals: TerminalRegistry,
}

impl AppState {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            // Long enough for a slow streaming turn; individual probes set
            // their own shorter timeout.
            .timeout(Duration::from_secs(300))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|err| format!("failed to build the HTTP client: {err}"))?;

        Ok(Self {
            http,
            backend: Mutex::new(None),
            boot: BootProgress::new(),
            bootstrap: BootstrapState::new(),
            terminals: Mutex::new(HashMap::new()),
        })
    }
}
