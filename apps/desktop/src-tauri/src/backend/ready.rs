//! The backend's port-announcement contract.
//!
//! `hermes serve` prints `HERMES_BACKEND_READY port=<N>` to stdout once uvicorn
//! has bound its socket; the legacy `hermes dashboard` runtime prints
//! `HERMES_DASHBOARD_READY port=<N>`. Both are accepted so a desktop shell
//! newer than the managed runtime still boots.
//!
//! Port of `electron/backend-ready.ts`. The two differences from the Node
//! version are deliberate:
//!   * no regex crate — the marker scan is hand-rolled, so `\w` semantics are
//!     reproduced explicitly with `char::is_alphanumeric` + `'_'`;
//!   * the buffer is polled by the caller instead of streamed through an
//!     event listener.

/// On a cold install the child must import the whole
/// `hermes_cli.main` → `web_server` → FastAPI/uvicorn chain before it binds,
/// and Windows real-time AV scans every freshly written `.pyc`. That pre-bind
/// cost can run 30-60s on a slow disk, so a tight deadline would kill a healthy
/// but still-starting backend and pile up orphans (#50209).
pub const DEFAULT_PORT_ANNOUNCE_TIMEOUT_MS: u64 = 90_000;

/// Never trust a deadline tighter than the warm-start path needs (#50209).
pub const MIN_PORT_ANNOUNCE_TIMEOUT_MS: u64 = 45_000;

const MARKERS: [&str; 2] = ["HERMES_BACKEND_READY port=", "HERMES_DASHBOARD_READY port="];

/// Resolve the port-announcement deadline, honoring
/// `HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS` and clamping to the floor so a
/// malformed override cannot make boot flakier than the default.
pub fn resolve_port_announce_timeout_ms() -> u64 {
    let raw = std::env::var("HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS").ok();

    let parsed = raw.and_then(|value| value.trim().parse::<f64>().ok());

    match parsed {
        Some(ms) if ms.is_finite() && ms > 0.0 => (ms.round() as u64).max(MIN_PORT_ANNOUNCE_TIMEOUT_MS),
        _ => DEFAULT_PORT_ANNOUNCE_TIMEOUT_MS,
    }
}

/// True when `ch` would make the marker read as the tail of a longer
/// identifier — a letter or `_`. Digits are deliberately NOT included: the
/// sentinel is legitimately spliced right after uvicorn's stderr
/// `...listening on 127.0.0.1:8630` line, whose tail is a digit, so a digit
/// preceding the marker is normal (#103792 spliced case, same class).
fn is_identifier_char(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

/// Scan accumulated stdout for the READY sentinel and return the announced
/// port.
///
/// Matches on a token boundary rather than `^`-anchored lines: uvicorn's stderr
/// chunks end without a newline, so the sentinel can be spliced onto the end of
/// an unrelated line (`...process [4711]HERMES_BACKEND_READY port=65238`) and a
/// line anchor would never line up (#103792). The boundary check rejects a
/// letter/underscore prefix (`XHERMES_BACKEND_READY`) but allows a digit
/// prefix, which is the `...:8630HERMES_BACKEND_READY` splicing case.
pub fn parse_ready_port(buffer: &str) -> Option<u16> {
    for marker in MARKERS {
        let mut cursor = 0usize;

        while let Some(offset) = buffer[cursor..].find(marker) {
            let at = cursor + offset;

            // `port=<digits>` already keeps prose mentions out; the boundary
            // check additionally rejects `XHERMES_BACKEND_READY port=1`.
            let boundary_ok = buffer[..at].chars().next_back().map_or(true, |ch| !is_identifier_char(ch));

            if boundary_ok {
                let digits: String = buffer[at + marker.len()..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit())
                    .collect();

                if !digits.is_empty() {
                    if let Ok(port) = digits.parse::<u16>() {
                        if port > 0 {
                            return Some(port);
                        }
                    }
                }
            }

            cursor = at + marker.len();
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_ready_line() {
        assert_eq!(parse_ready_port("HERMES_BACKEND_READY port=65238\n"), Some(65238));
    }

    #[test]
    fn parses_dashboard_ready_line() {
        assert_eq!(parse_ready_port("HERMES_DASHBOARD_READY port=9119"), Some(9119));
    }

    #[test]
    fn parses_sentinel_spliced_onto_uvicorn_stderr() {
        // The exact #103792 shape: no newline before the sentinel.
        assert_eq!(
            parse_ready_port("INFO:     Application startup complete. [4711]HERMES_BACKEND_READY port=65238"),
            Some(65238)
        );
    }

    #[test]
    fn parses_sentinel_spliced_after_a_port_number() {
        // The shape that broke boot: uvicorn's stderr "listening on
        // 127.0.0.1:8630" line ends in a digit, and the stdout sentinel lands
        // right after it with no newline. A letter/underscore prefix must still
        // be rejected; a digit prefix must NOT.
        assert_eq!(
            parse_ready_port("Hermes backend listening on 127.0.0.1:8630HERMES_BACKEND_READY port=8630"),
            Some(8630)
        );
    }

    #[test]
    fn rejects_sentinel_without_token_boundary() {
        assert_eq!(parse_ready_port("XHERMES_BACKEND_READY port=1234"), None);
    }

    #[test]
    fn rejects_prose_mention_without_port() {
        assert_eq!(parse_ready_port("waiting for HERMES_BACKEND_READY to arrive"), None);
    }

    #[test]
    fn ignores_zero_port() {
        assert_eq!(parse_ready_port("HERMES_BACKEND_READY port=0"), None);
    }

    #[test]
    fn timeout_floor_is_enforced() {
        std::env::set_var("HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS", "1000");
        assert_eq!(resolve_port_announce_timeout_ms(), MIN_PORT_ANNOUNCE_TIMEOUT_MS);
        std::env::remove_var("HERMES_DESKTOP_PORT_ANNOUNCE_TIMEOUT_MS");
    }
}
