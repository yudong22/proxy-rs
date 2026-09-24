//! Small shared helpers used across the proxy, stats and credits modules.

use axum::{
    body::Body,
    http::{HeaderMap, Request},
};
use bytes::Bytes;
use futures::stream::StreamExt;

/// Truncate `text` to at most `max` characters, appending an ellipsis when cut.
///
/// Character-based (never byte-based) so it cannot split a multi-byte UTF-8
/// sequence and panic, which is why every caller funnels through here instead
/// of slicing strings directly.
pub fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut end = 0;
    for (i, ch) in text.char_indices() {
        if i > max {
            break;
        }
        end = i + ch.len_utf8();
    }
    format!("{}…", &text[..end])
}

/// Render a header map as a single `k: v | k: v` log line.
///
/// Credential-bearing headers are redacted first. These lines are written to
/// `~/.proxy-rs/logs/proxy.log`, which is long-lived and routinely copied into
/// bug reports, so an upstream API key must never land there in the clear. The
/// leading characters are kept so a key can still be told apart from another
/// while debugging.
pub fn format_headers(headers: &HeaderMap) -> String {
    headers
        .iter()
        .map(|(k, v)| {
            let value = v.to_str().unwrap_or("<non-utf8>");
            format!("{}: {}", k, redact_header_value(k.as_str(), value))
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Header names whose value is a credential and must not be logged verbatim.
///
/// Matched exactly (case-insensitively) or by the `contains` rule below — see
/// [`is_sensitive_header`]. Providers do not agree on a single spelling, so the
/// common vendor variants are listed rather than relying on one canonical name.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "cookie",
    "set-cookie",
    "x-goog-api-key",
    "openai-api-key",
    "anthropic-api-key",
    "x-anthropic-api-key",
    "x-api-token",
];

/// Substrings that mark a header as credential-bearing even when the exact name
/// is not listed (`x-some-vendor-api-key`, `x-foo-token`, …).
const SENSITIVE_NAME_FRAGMENTS: &[&str] = &["api-key", "apikey", "auth-token", "access-token"];

fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if SENSITIVE_HEADERS.contains(&name.as_str()) {
        return true;
    }
    if name.ends_with("-token") || name.ends_with("-secret") {
        return true;
    }
    SENSITIVE_NAME_FRAGMENTS.iter().any(|f| name.contains(f))
}

/// Mask a credential, keeping a short prefix for identification.
///
/// `Bearer ck_abc…` keeps its scheme so the line still reads naturally; a bare
/// token keeps a very short prefix. The prefix is deliberately small and the
/// value is masked entirely when that prefix would be a meaningful fraction of
/// the secret — these lines end up in `proxy.log`, which is routinely attached
/// to bug reports.
fn redact_header_value(name: &str, value: &str) -> String {
    if !is_sensitive_header(name) {
        return value.to_string();
    }

    // Any `<scheme> <secret>` form: `Bearer …`, `Basic …`, `ApiKey …`, `SSWS …`.
    // Only a leading token followed by whitespace is treated as a scheme; a
    // bare secret containing spaces keeps its whitespace inside the mask.
    let (scheme, secret) = match value.split_once(' ') {
        Some((scheme, rest))
            if !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                && rest.contains(|c: char| !c.is_whitespace()) =>
        {
            (Some(scheme), rest.trim())
        }
        _ => (None, value.trim()),
    };

    // Below this length a visible prefix would be a meaningful fraction of the
    // secret, so show nothing at all.
    const KEEP: usize = 4;
    let masked = if secret.chars().count() > KEEP + 8 {
        let prefix: String = secret.chars().take(KEEP).collect();
        format!("{prefix}…")
    } else {
        "…".to_string()
    };

    match scheme {
        Some(scheme) => format!("{scheme} {masked}"),
        None => masked,
    }
}

/// Default cap on the request body the proxy will buffer, in bytes (32 MiB).
///
/// The figure is a memory guard, not an API limit: Claude Code's own request
/// bodies already reach ~2 MB on a long session, so axum's 2 MB default rejects
/// legitimate traffic. 32 MiB leaves an order of magnitude of headroom while
/// still bounding what one request can pin.
pub const DEFAULT_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Environment override for [`DEFAULT_MAX_BODY_BYTES`], in bytes.
pub const MAX_BODY_ENV: &str = "PROXY_MAX_BODY_BYTES";
pub const LEGACY_MAX_BODY_ENV: &str = "ANTHROPIC_PROXY_MAX_BODY_BYTES";

/// The configured body cap, read from the environment on each call.
///
/// A blank, non-numeric or zero value falls back to the default rather than
/// disabling the limit: an unparseable override must not become "unlimited".
pub fn max_body_bytes() -> usize {
    std::env::var(MAX_BODY_ENV)
        .or_else(|_| std::env::var(LEGACY_MAX_BODY_ENV))
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

/// Render a byte count for an error message (`2 MiB`, `512 KiB`, `300 bytes`).
pub fn human_bytes(n: usize) -> String {
    const MIB: usize = 1024 * 1024;
    const KIB: usize = 1024;
    if n >= MIB && n.is_multiple_of(MIB) {
        format!("{} MiB", n / MIB)
    } else if n >= KIB && n.is_multiple_of(KIB) {
        format!("{} KiB", n / KIB)
    } else {
        format!("{n} bytes")
    }
}

/// Why a request body could not be buffered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    /// The body was larger than the configured cap.
    TooLarge { limit: usize },
    /// The body could not be read at all (client disconnect, reset stream, …).
    Io(String),
}

impl BodyError {
    /// The status and message the client is told.
    pub fn parts(&self) -> (u16, String) {
        match self {
            BodyError::TooLarge { limit } => (
                413,
                format!(
                    "request body exceeds the {} limit (raise {MAX_BODY_ENV} to allow larger bodies)",
                    human_bytes(*limit)
                ),
            ),
            BodyError::Io(detail) => (
                400,
                format!("could not read the request body: {detail}"),
            ),
        }
    }
}

/// A buffered request body: what it parsed to, whether it buffered at all, and
/// the request handed back for the next consumer.
///
/// `json` is a *logging* aid — the typed extraction that follows is what
/// validates the payload — so a parse failure here is not itself an error. Only
/// [`BodyError`] means the body never arrived, and that must be answered rather
/// than passed on: re-reading a body that already failed would report a
/// misleading "invalid JSON" instead of what actually went wrong.
pub struct PeekedBody {
    pub json: Option<serde_json::Value>,
    pub error: Option<BodyError>,
    pub request: Request<Body>,
}

/// Parse a JSON body from the exact bytes the client sent.
///
/// Used at the top of a handler, before `Json<T>` consumes the body: `T` is the
/// *translation* model, which deliberately drops fields this proxy does not
/// forward, so the typed request cannot be the source of truth for something
/// the client told us (e.g. Claude Code's `metadata.user_id`). Reading the raw
/// bytes keeps that information without widening a translation type for what is
/// purely a logging concern.
///
/// `limit` bounds what is buffered here as well as what the extractor accepts
/// (the router installs the same value as axum's `DefaultBodyLimit`), so a body
/// over the cap is rejected once, with a status and message this proxy chose,
/// instead of being read into memory unbounded and then failing in the extractor.
pub async fn peek_json_body(req: Request<Body>, limit: usize) -> PeekedBody {
    let (parts, body) = req.into_parts();
    match collect_bounded(body, limit).await {
        Ok(bytes) => {
            // A non-JSON body is not fatal here: `Json` reports it with a far
            // better message than this peek could, so the error is dropped.
            let json = serde_json::from_slice(&bytes).ok();
            PeekedBody {
                json,
                error: None,
                request: Request::from_parts(parts, Body::from(bytes)),
            }
        }
        Err(error) => PeekedBody {
            json: None,
            error: Some(error),
            request: Request::from_parts(parts, Body::empty()),
        },
    }
}

/// Read a body, refusing to buffer more than `limit` bytes.
///
/// Written out rather than delegated to `axum::body::to_bytes` for two reasons:
/// the failure mode has to be distinguishable (an over-limit body is a 413, a
/// dropped connection is not), and `to_bytes` reports both as one opaque error
/// whose length-limit variant lives in a crate this one does not depend on.
/// Counting here keeps the distinction explicit and local.
///
/// The limit is checked against the *sum* accumulated so far, not per frame: a
/// chunked body arrives in arbitrary pieces, so a per-frame check would pass a
/// body of any size as long as every individual frame stayed small.
async fn collect_bounded(body: Body, limit: usize) -> Result<Bytes, BodyError> {
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();

    while let Some(frame) = stream.next().await {
        let frame = frame.map_err(|e| BodyError::Io(e.to_string()))?;
        if buf.len() + frame.len() > limit {
            return Err(BodyError::TooLarge { limit });
        }
        buf.extend_from_slice(&frame);
    }

    Ok(Bytes::from(buf))
}

/// Turn a failed typed extraction into an error that keeps its real status.
///
/// The rejection already carries the right code — 400 for unparseable JSON, 415
/// for a wrong `content-type`, 422 for a shape mismatch — so it is preserved
/// rather than funnelled into a generic 400. The route is named because three
/// different APIs share one port and the client should not have to guess which
/// one refused it.
pub fn rejection_error(
    route: &str,
    rejection: &axum::extract::rejection::JsonRejection,
) -> crate::error::ProxyError {
    crate::error::ProxyError::Rejected {
        status: rejection.status().as_u16(),
        message: format!(
            "invalid {route} request: {} (HTTP {})",
            rejection.body_text(),
            rejection.status().as_u16()
        ),
    }
}

/// Convert days since the Unix epoch to a `(year, month, day)` civil date.
///
/// Howard Hinnant's `civil_from_days` algorithm, shared by the log timestamp,
/// daily-stats and credits date rendering.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format epoch seconds as a local `YYYY-MM-DD HH:MM:SS` string (UTC offset
/// applied by the caller). Avoids pulling in `chrono`.
pub fn format_epoch_secs(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

/// Local UTC offset in seconds, resolved once per call from the system zone.
///
/// Resolution order:
/// 1. `PROXY_TZ_OFFSET_HOURS` — explicit override, e.g. `"8"` for UTC+8.
/// 2. The system timezone, via `localtime_r`. This is what makes timestamps
///    follow the machine's zone instead of being hard-coded to +8; the result
///    includes DST, so it is re-read rather than assumed constant.
/// 3. UTC (0) if the system lookup somehow fails.
pub fn local_utc_offset_secs() -> i64 {
    if let Ok(v) = std::env::var("PROXY_TZ_OFFSET_HOURS") {
        if let Ok(h) = v.trim().parse::<i64>() {
            return h * 3600;
        }
    }
    system_utc_offset_secs().unwrap_or(0)
}

/// Ask libc for the current zone's UTC offset (e.g. 28800 for Asia/Shanghai).
///
/// `localtime_r` + `tm_gmtoff` is the direct answer on Unix, but Windows'
/// `libc::tm` has no `tm_gmtoff` field, so that path is `cfg`-gated and
/// Windows falls back to deriving the offset from the broken-down local time.
#[cfg(unix)]
fn system_utc_offset_secs() -> Option<i64> {
    let secs = now_secs()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a valid `libc::tm` out-param; `localtime_r` fills it on
    // success and returns null (without touching it) on failure.
    let ok = unsafe { libc::localtime_r(&(secs as libc::time_t), &mut tm) };
    if ok.is_null() {
        return None;
    }
    Some(tm.tm_gmtoff as i64)
}

/// Windows has no `tm_gmtoff`, so derive the offset by asking for both the
/// local and UTC broken-down time and measuring the difference.
///
/// The day fields matter, not just the time-of-day: UTC+13 and UTC-11 both show
/// a 1-hour-vs-12-hour split, and only the date tells them apart.
#[cfg(not(unix))]
fn system_utc_offset_secs() -> Option<i64> {
    let secs = now_secs()?;
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let mut utc: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both are valid `libc::tm` out-params, as above.
    if unsafe { libc::localtime_r(&(secs as libc::time_t), &mut local) }.is_null() {
        return None;
    }
    if unsafe { libc::gmtime_r(&(secs as libc::time_t), &mut utc) }.is_null() {
        return None;
    }

    let local_days = days_from_civil(
        local.tm_year as i64 + 1900,
        local.tm_mon as i64 + 1,
        local.tm_mday as i64,
    );
    let utc_days = days_from_civil(
        utc.tm_year as i64 + 1900,
        utc.tm_mon as i64 + 1,
        utc.tm_mday as i64,
    );
    let local_sod = local.tm_hour as i64 * 3600 + local.tm_min as i64 * 60 + local.tm_sec as i64;
    let utc_sod = utc.tm_hour as i64 * 3600 + utc.tm_min as i64 * 60 + utc.tm_sec as i64;

    Some((local_days - utc_days) * 86_400 + (local_sod - utc_sod))
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
#[cfg(not(unix))]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_secs() -> Option<i64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if secs > i64::MAX as u64 {
        return None;
    }
    Some(secs as i64)
}

/// Format the current instant as a local `YYYY-MM-DD HH:MM:SS.mmm` string.
///
/// This is the single timestamp source for log lines and stats rows, so both
/// agree and both honour the machine's timezone.
pub fn local_datetime_millis() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let local = (now.as_secs() as i64 + local_utc_offset_secs()).max(0) as u64;
    format!("{}.{:03}", format_epoch_secs(local), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_text_and_marks_long_text() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("  hi  ", 10), "hi");
        assert!(truncate("hello world", 5).ends_with('…'));
    }

    #[test]
    fn truncate_does_not_split_multibyte_characters() {
        // Each char is 3 bytes; a byte-indexed cut here would panic.
        let text = "中文中文中文";
        let out = truncate(text, 4);
        assert!(out.ends_with('…'));
        assert!(out.starts_with('中'));
    }

    #[test]
    fn format_headers_redacts_credentials() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static(
                "Bearer ck_fm3j4t8apekg.AtU_2TMOY8pdXrOmXHTJPkm-hSbuLhroRjABd8flTgQ",
            ),
        );
        headers.insert(
            "x-api-key",
            HeaderValue::from_static("sk-ant-secret-value-here"),
        );
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let line = format_headers(&headers);

        assert!(
            !line.contains("AtU_2TMOY8pdXrOmXHTJPkm"),
            "key leaked: {line}"
        );
        assert!(!line.contains("secret-value-here"), "key leaked: {line}");
        assert!(
            line.contains("Bearer ck_f…"),
            "scheme+short prefix kept: {line}"
        );
        assert!(
            line.contains("content-type: application/json"),
            "others intact"
        );
    }

    #[test]
    fn format_headers_redacts_non_bearer_schemes_and_vendor_names() {
        use axum::http::HeaderValue;
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_static("AIzaSyD-very-secret-google-key"),
        );
        headers.insert(
            "x-vendor-access-token",
            HeaderValue::from_static("vendor-token-value-123456"),
        );
        headers.insert(
            "authorization",
            HeaderValue::from_static("Basic dXNlcjpwYXNzd29yZA=="),
        );

        let line = format_headers(&headers);

        for secret in [
            "SyD-very-secret-google-key",
            "token-value-123456",
            "dXNlcjpwYXNzd29yZA",
        ] {
            assert!(!line.contains(secret), "leaked {secret}: {line}");
        }
        assert!(line.contains("Basic "), "scheme kept: {line}");
    }

    #[test]
    fn short_secrets_are_masked_entirely() {
        use axum::http::HeaderValue;
        // 12 chars: a visible prefix would be a meaningful fraction of it.
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("shortsecret1"));
        let line = format_headers(&headers);
        assert!(!line.contains("short"), "leaked: {line}");
        assert!(!line.contains("secret1"), "leaked: {line}");
    }

    #[test]
    fn redaction_keeps_nothing_when_the_value_is_too_short() {
        // A short secret would be given away by any visible prefix.
        assert_eq!(redact_header_value("authorization", "abc"), "…");
    }

    #[test]
    fn redaction_leaves_non_credential_headers_alone() {
        assert_eq!(
            redact_header_value("user-agent", "deepseek-harness/0.1.6-alpha.2"),
            "deepseek-harness/0.1.6-alpha.2"
        );
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
    }

    #[test]
    fn format_epoch_secs_renders_utc_components() {
        assert_eq!(format_epoch_secs(0), "1970-01-01 00:00:00");
        assert_eq!(format_epoch_secs(86_400 + 3661), "1970-01-02 01:01:01");
    }

    #[test]
    fn human_bytes_picks_a_readable_unit() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(4 * 1024), "4 KiB");
        assert_eq!(human_bytes(32 * 1024 * 1024), "32 MiB");
        // A non-round figure keeps the exact byte count rather than rounding to
        // a unit and hiding what the limit actually is.
        assert_eq!(human_bytes(4096 + 1), "4097 bytes");
    }

    /// Serialises the tests that touch the process-global body cap.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The cap must never silently become "unlimited": an editor writing a blank
    /// or junk value is the realistic way that happens.
    #[test]
    fn max_body_bytes_falls_back_on_bad_overrides() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::remove_var(LEGACY_MAX_BODY_ENV);

        for bad in ["", "  ", "0", "not-a-number", "-1"] {
            std::env::set_var(MAX_BODY_ENV, bad);
            assert_eq!(
                max_body_bytes(),
                DEFAULT_MAX_BODY_BYTES,
                "{bad:?} must fall back to the default"
            );
        }

        std::env::set_var(MAX_BODY_ENV, " 65536 ");
        assert_eq!(max_body_bytes(), 65536, "a padded number is still honoured");

        std::env::remove_var(MAX_BODY_ENV);
        assert_eq!(max_body_bytes(), DEFAULT_MAX_BODY_BYTES);

        // Legacy variable also works when new one is unset
        std::env::set_var(LEGACY_MAX_BODY_ENV, "131072");
        assert_eq!(max_body_bytes(), 131072);
        std::env::remove_var(LEGACY_MAX_BODY_ENV);
    }

    #[tokio::test]
    async fn collect_bounded_accepts_a_body_exactly_at_the_limit() {
        let bytes = collect_bounded(Body::from(vec![b'a'; 64]), 64)
            .await
            .expect("a body equal to the limit is allowed");
        assert_eq!(bytes.len(), 64);
    }

    /// The limit is a total, not a per-frame rule: a body split into small
    /// frames must still be refused once their sum passes it.
    #[tokio::test]
    async fn collect_bounded_counts_across_frames() {
        let frames: Vec<Result<Bytes, std::io::Error>> = (0..8)
            .map(|_| Ok(Bytes::from_static(&[b'x'; 16])))
            .collect();
        let body = Body::from_stream(futures::stream::iter(frames));

        // 8 frames x 16 bytes = 128, against a 64-byte cap. Each frame is well
        // under the limit on its own.
        let err = collect_bounded(body, 64)
            .await
            .expect_err("the sum must be what is bounded");
        assert_eq!(err, BodyError::TooLarge { limit: 64 });
    }

    #[tokio::test]
    async fn collect_bounded_reports_a_read_failure_as_io() {
        let body = Body::from_stream(futures::stream::iter(vec![Err::<Bytes, std::io::Error>(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "gone"),
        )]));

        let err = collect_bounded(body, 1024)
            .await
            .expect_err("a failed read is an error");
        assert!(matches!(err, BodyError::Io(_)), "got {err:?}");
        assert_eq!(err.parts().0, 400, "a failed read is not a size problem");
    }

    #[tokio::test]
    async fn peek_hands_the_bytes_back_to_the_next_consumer() {
        let body = serde_json::json!({"model": "m", "messages": []}).to_string();
        let request = Request::builder()
            .header("content-type", "application/json")
            .body(Body::from(body.clone()))
            .unwrap();

        let peeked = peek_json_body(request, 1024).await;

        assert!(peeked.error.is_none());
        assert_eq!(peeked.json.unwrap()["model"], "m");
        // The extractor downstream must still see the original body.
        let rest = axum::body::to_bytes(peeked.request.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&rest), body);
    }

    /// A body that fails to buffer is not silently turned into an empty one:
    /// the caller has to notice `error` and answer it.
    #[tokio::test]
    async fn peek_marks_an_oversized_body_and_does_not_guess_at_json() {
        let request = Request::builder()
            .header("content-type", "application/json")
            .body(Body::from(vec![b'x'; 2048]))
            .unwrap();

        let peeked = peek_json_body(request, 128).await;

        assert_eq!(peeked.error, Some(BodyError::TooLarge { limit: 128 }));
        assert!(peeked.json.is_none(), "an unbuffered body cannot be parsed");
    }
}
