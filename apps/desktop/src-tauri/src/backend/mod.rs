//! Desktop-managed backend lifecycle.
//!
//! Split the same way the Electron main process split it, so the pieces stay
//! independently testable: `command` (argv + executable resolution), `env`
//! (spawn environment), `ready` (port announcement), `manager` (spawn + probe +
//! teardown, and the connection descriptor the renderer consumes).

pub mod command;
pub mod env;
pub mod manager;
pub mod ready;

pub use manager::{
    descriptor_for, gateway_ws_url, is_alive, serves_profile, spawn_backend, stop_backend,
    wait_for_ready, BackendHandle, ConnectionDescriptor,
};
