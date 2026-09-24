use metrics::{counter, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::OnceLock;
use std::time::Instant;

/// Re-exported so callers can name the handle type without depending on the
/// third-party exporter crate directly.
pub use metrics_exporter_prometheus::PrometheusHandle;

/// The global Prometheus recorder can only be installed once per process.
/// Cache the handle so repeated calls (e.g. every proxy (re)start from the GUI)
/// return the same recorder instead of panicking.
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

pub fn install() -> PrometheusHandle {
    PROMETHEUS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install Prometheus recorder")
        })
        .clone()
}

pub fn request_started(streaming: bool) {
    let mode = if streaming { "streaming" } else { "batch" };
    counter!("proxy_requests_total", "mode" => mode).increment(1);
    gauge!("proxy_requests_in_flight").increment(1.0);
}

/// Ties the in-flight gauge to a scope, so it cannot be left incremented.
///
/// `request_started` bumps `proxy_requests_in_flight` before the request is
/// translated, and translation can fail with `?` — a return that never reaches
/// `request_finished`, permanently inflating the gauge. Holding this for the
/// request's lifetime makes the decrement unconditional: whichever path leaves
/// the handler, the gauge comes back down exactly once.
pub struct InFlightGuard {
    streaming: bool,
    finished: bool,
}

impl InFlightGuard {
    /// Record the start and return the guard that will balance it.
    pub fn start(streaming: bool) -> Self {
        request_started(streaming);
        Self {
            streaming,
            finished: false,
        }
    }

    /// Record the finished request and consume the guard.
    pub fn finish(mut self, start: Instant, status: u16) {
        self.finished = true;
        request_finished(start, status, self.streaming);
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Only reached when the request left without finishing, so the gauge
        // alone needs balancing — there is no status or duration to record.
        if !self.finished {
            gauge!("proxy_requests_in_flight").decrement(1.0);
        }
    }
}

pub fn request_finished(start: Instant, status: u16, streaming: bool) {
    let mode = if streaming { "streaming" } else { "batch" };
    let status_str = status.to_string();

    histogram!("proxy_request_duration_seconds", "mode" => mode, "status" => status_str.clone())
        .record(start.elapsed().as_secs_f64());
    counter!("proxy_responses_total", "mode" => mode, "status" => status_str).increment(1);
    gauge!("proxy_requests_in_flight").decrement(1.0);
}

pub fn upstream_latency(seconds: f64, endpoint: &'static str) {
    histogram!("proxy_upstream_latency_seconds", "endpoint" => endpoint).record(seconds);
}

pub fn tokens(input: u32, output: u32, model: &str) {
    let model = model.to_string();
    counter!("proxy_tokens_total", "type" => "input", "model" => model.clone())
        .increment(input as u64);
    counter!("proxy_tokens_total", "type" => "output", "model" => model).increment(output as u64);
}

pub fn upstream_error(endpoint: &'static str) {
    counter!("proxy_upstream_errors_total", "endpoint" => endpoint).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GUI re-installs the recorder on every proxy (re)start. This must be
    /// idempotent: previously a second call panicked, which silently aborted the
    /// server task so the port was never bound and nothing was logged.
    #[test]
    fn install_is_idempotent() {
        let a = install();
        let b = install();
        // Both calls must return a usable handle rather than panicking.
        let _ = a.render();
        let _ = b.render();
    }

    /// Serializes the gauge tests: `proxy_requests_in_flight` is process-global,
    /// so tests running in parallel would see each other's increments. Each test
    /// holds this for its whole body rather than only around its reads.
    static GAUGE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_gauge_tests() -> std::sync::MutexGuard<'static, ()> {
        GAUGE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The in-flight gauge as the exporter renders it, for the assertions below.
    fn in_flight_gauge() -> f64 {
        let rendered = install().render();
        rendered
            .lines()
            .find_map(|line| {
                line.strip_prefix("proxy_requests_in_flight ")
                    .and_then(|v| v.trim().parse::<f64>().ok())
            })
            .unwrap_or(0.0)
    }

    #[test]
    fn a_finished_request_returns_the_gauge_to_its_starting_value() {
        let _lock = lock_gauge_tests();
        let before = in_flight_gauge();
        InFlightGuard::start(false).finish(Instant::now(), 200);
        assert_eq!(
            in_flight_gauge(),
            before,
            "a completed request must not leave the gauge incremented"
        );
    }

    #[test]
    fn an_abandoned_request_still_returns_the_gauge() {
        let _lock = lock_gauge_tests();
        // The regression: a translation failure returned with `?`, so
        // `request_finished` never ran and the gauge drifted up permanently.
        let before = in_flight_gauge();
        {
            let _guard = InFlightGuard::start(true);
            // Dropped without `finish`, as an early return would.
        }
        assert_eq!(
            in_flight_gauge(),
            before,
            "an early return must still balance the gauge"
        );
    }

    #[test]
    fn the_gauge_tracks_concurrent_requests() {
        let _lock = lock_gauge_tests();
        let before = in_flight_gauge();
        let a = InFlightGuard::start(false);
        let b = InFlightGuard::start(true);
        assert_eq!(in_flight_gauge(), before + 2.0, "both must be counted");

        drop(a);
        assert_eq!(in_flight_gauge(), before + 1.0, "only the live one remains");

        b.finish(Instant::now(), 200);
        assert_eq!(in_flight_gauge(), before);
    }
}
