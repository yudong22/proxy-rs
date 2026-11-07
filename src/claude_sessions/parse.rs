//! Line-level parsing helpers: timestamps, JSON field access, text capping.

use super::*;

/// `"2026-10-25T11:34:10.152Z"` → epoch ms. Accepts 0–9 fractional digits and
/// an optional `+HH:MM` / `-HH:MM` offset.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = s.get(range)?;
        if slice.bytes().all(|b| b.is_ascii_digit()) {
            slice.parse().ok()
        } else {
            None
        }
    };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    // Fractional seconds, then the offset.
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() > 9 {
            return None;
        }
        let padded = format!("{digits:0<3}");
        millis = padded[..3].parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_minutes = match rest {
        "Z" | "z" | "" => 0,
        other => {
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &other[1..];
            let (h, m) = if let Some((h, m)) = body.split_once(':') {
                (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
            } else if body.len() == 4 {
                (body[..2].parse().ok()?, body[2..].parse().ok()?)
            } else {
                return None;
            };
            sign * (h * 60 + m)
        }
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(secs * 1_000 + millis)
}

/// Howard Hinnant's `days_from_civil`, valid for the whole proleptic period.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether a `user` text is a real prompt rather than injected machinery.
///
/// Claude Code writes system reminders, background task notifications and
/// slash-command wrappers into the transcript as plain user strings. Counting
/// them would make every session look several times longer than it was.
pub fn human_prompt(text: &str) -> bool {
    let t = text.trim_start();
    if t.is_empty() {
        return false;
    }
    const NOISE: &[&str] = &[
        "<task-notification>",
        "<system-reminder>",
        "<local-command-stdout>",
        "<local-command-caveat>",
        "<command-name>",
        "Caveat: The messages below",
        "<user-prompt-submit-hook>",
    ];
    !NOISE.iter().any(|p| t.starts_with(p))
}

pub(crate) fn cap_chars(s: &str, cap: usize) -> (String, bool) {
    match s.char_indices().nth(cap) {
        Some((idx, _)) => (s[..idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

pub(crate) fn ts_of(v: &serde_json::Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_iso_ms)
}

pub(crate) fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn i64_of(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// `usage`, tolerating the (observed) case of it being a JSON-encoded string.
pub(crate) fn usage_of(message: &serde_json::Value) -> Option<TokenTotals> {
    let usage = match message.get("usage")? {
        serde_json::Value::Object(_) => message.get("usage")?,
        serde_json::Value::String(s) => {
            let parsed: serde_json::Value = serde_json::from_str(s).ok()?;
            return usage_from_object(&parsed);
        }
        _ => return None,
    };
    usage_from_object(usage)
}

fn usage_from_object(u: &serde_json::Value) -> Option<TokenTotals> {
    if !u.is_object() {
        return None;
    }
    let thinking = u
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    Some(TokenTotals {
        input: i64_of(u, "input_tokens"),
        output: i64_of(u, "output_tokens"),
        cache_read: i64_of(u, "cache_read_input_tokens"),
        cache_write: i64_of(u, "cache_creation_input_tokens"),
        thinking,
    })
}

// ── Accumulator ─────────────────────────────────────────────────────────

pub(crate) fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(200));
    for c in s.chars() {
        if c == '\n' || c == '\r' || c == '\t' {
            out.push(' ');
        } else {
            out.push(c);
        }
        if out.len() >= 400 {
            break;
        }
    }
    out.trim().to_string()
}

pub(crate) fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| str_of(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn parse_cost_state(v: &serde_json::Value) -> SessionCostState {
    let f64_of = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut models = Vec::new();
    if let Some(map) = v.get("modelUsage").and_then(|m| m.as_object()) {
        for (name, entry) in map {
            models.push(ModelUsage {
                model: name.clone(),
                turns: 0,
                tokens: TokenTotals {
                    input: i64_of(entry, "inputTokens"),
                    output: i64_of(entry, "outputTokens"),
                    cache_read: i64_of(entry, "cacheReadInputTokens"),
                    cache_write: i64_of(entry, "cacheCreationInputTokens"),
                    thinking: i64_of(entry, "thinkingTokens"),
                },
            });
        }
    }
    models.sort_by(|a, b| {
        b.tokens
            .output
            .cmp(&a.tokens.output)
            .then(a.model.cmp(&b.model))
    });
    SessionCostState {
        total_cost_usd: f64_of("totalCostUSD"),
        total_duration_ms: i64_of(v, "totalDuration"),
        api_duration_ms: i64_of(v, "totalAPIDuration"),
        tool_duration_ms: i64_of(v, "totalToolDuration"),
        start_time_ms: v.get("startTime").and_then(|x| x.as_i64()),
        lines_added: i64_of(v, "totalLinesAdded"),
        lines_removed: i64_of(v, "totalLinesRemoved"),
        has_unknown_model_cost: v
            .get("hasUnknownModelCost")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        models,
    }
}

// ── Reading ─────────────────────────────────────────────────────────────

/// Iterate over complete lines of a reader, skipping (and counting) any single
/// line longer than [`MAX_LINE_BYTES`].
pub(crate) fn read_lines<R: BufRead>(reader: R, mut on_line: impl FnMut(&str)) -> Result<()> {
    let mut reader = reader;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let mut limited = (&mut reader).take(MAX_LINE_BYTES + 1);
        let n = limited.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if n as u64 > MAX_LINE_BYTES {
            // Drain the rest of the oversized line so the next read starts at
            // a line boundary.
            if buf.last() != Some(&b'\n') {
                let mut rest = Vec::new();
                let _ = reader.read_until(b'\n', &mut rest);
            }
            on_line("\u{0}oversized");
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches(['\n', '\r']);
        on_line(text);
    }
    Ok(())
}

pub(crate) fn file_mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
