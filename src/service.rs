use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// Shared handle to the proxy service (the `/v1/*` listener).
///
/// The console is served on the *same* listener as the proxy API, so stopping
/// the service from the GUI must not take the console offline: the listener is
/// torn down and re-bound by the caller, while this handle tracks the state the
/// UI reads.
pub struct ServiceController {
    running: AtomicBool,
    /// Whether to retry `preferred + 1` when the preferred port is taken.
    ///
    /// Off in the desktop app: the client CLIs are configured against one fixed
    /// gateway URL, so silently binding a different port breaks them with no
    /// visible cause. A busy port is surfaced as an error instead, and the
    /// single-instance guard is what actually prevents the conflict (see
    /// `src-tauri/src/main.rs`).
    allow_port_fallback: AtomicBool,
}

impl ServiceController {
    pub fn new(allow_port_fallback: bool) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            allow_port_fallback: AtomicBool::new(allow_port_fallback),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn allow_port_fallback(&self) -> bool {
        self.allow_port_fallback.load(Ordering::SeqCst)
    }

    pub fn mark_running(&self) {
        self.running.store(true, Ordering::SeqCst);
    }

    /// Mark the service as stopped so the GUI reflects reality.
    pub fn mark_stopped(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Bind the preferred port, falling back to `preferred + 1` when allowed.
    ///
    /// `allow_fallback` is passed in rather than read from the controller so
    /// the caller decides per bind; the desktop app passes `false`, which makes
    /// a busy port a hard error instead of a silent retarget.
    pub async fn bind(
        bind_addr: &str,
        preferred: u16,
        allow_fallback: bool,
    ) -> std::io::Result<(tokio::net::TcpListener, u16)> {
        match tokio::net::TcpListener::bind(format!("{bind_addr}:{preferred}")).await {
            Ok(l) => Ok((l, preferred)),
            Err(e) if allow_fallback => {
                let fallback = preferred.saturating_add(1);
                eprintln!("⚠️  Port {preferred} is busy ({e}), falling back to {fallback}");
                let l = tokio::net::TcpListener::bind(format!("{bind_addr}:{fallback}")).await?;
                Ok((l, fallback))
            }
            Err(e) => Err(e),
        }
    }
}

/// The JSON body returned while the service is stopped, so a client talking to
/// the shared listener gets a clear 503 rather than a connection reset.
pub fn service_unavailable_response() -> Response {
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header("content-type", "application/json")
        .body(
            serde_json::json!({
                "error": {
                    "type": "service_unavailable",
                    "message": "Proxy service is stopped. Start it from the console to continue.",
                    "code": "service_unavailable"
                }
            })
            .to_string(),
        )
        .expect("static response builds")
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_app_does_not_fall_back_to_another_port() {
        // A busy port must be a hard error, not a silent retarget: every client
        // CLI is pointed at one fixed gateway URL, so binding `port + 1` breaks
        // them with no visible cause. A developer who needs a second concurrent
        // instance changes the configured port instead.
        let ctrl = ServiceController::new(false);
        assert!(!ctrl.allow_port_fallback());
    }

    #[tokio::test]
    async fn bind_reports_a_busy_port_instead_of_moving_when_fallback_is_off() {
        // Occupy a port, then ask for it with fallback disabled.
        let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = held.local_addr().unwrap().port();

        let err = ServiceController::bind("127.0.0.1", port, false)
            .await
            .expect_err("a busy port must be an error when fallback is off");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn bind_falls_back_only_when_explicitly_asked() {
        // The fallback port is `preferred + 1`, so the test needs a pair where
        // the second port is genuinely free. Asking the OS for an ephemeral
        // port and assuming `port + 1` is unused is racy: another test or
        // process can hold it, and the assertion then fails intermittently.
        let (_held, port) = loop {
            let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = held.local_addr().unwrap().port();
            if port == u16::MAX {
                continue;
            }
            // Probe the neighbour, then release it so `bind` can take it.
            match tokio::net::TcpListener::bind(("127.0.0.1", port + 1)).await {
                Ok(probe) => {
                    drop(probe);
                    break (held, port);
                }
                Err(_) => continue,
            }
        };

        let (_listener, bound) = ServiceController::bind("127.0.0.1", port, true)
            .await
            .expect("fallback should find the next port");
        assert_eq!(bound, port + 1);
    }

    #[test]
    fn the_running_flag_tracks_state() {
        let ctrl = ServiceController::new(false);
        assert!(!ctrl.is_running());

        ctrl.mark_running();
        assert!(ctrl.is_running());

        ctrl.mark_stopped();
        assert!(!ctrl.is_running());
    }
}
