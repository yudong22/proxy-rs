//! Pure parsing: timestamps, segment merging, per-vendor extractors. No I/O.

use super::*;

/// Format epoch seconds as a local "YYYY-MM-DD HH:MM:SS" string without
/// pulling in `chrono`. Good enough for the upstream date-range filter.
pub(crate) fn chrono_like(epoch: i64, _fmt: &str) -> String {
    let days = epoch / 86_400;
    let rem = epoch % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    crate::util::civil_from_days(z)
}

/// Sum the remaining balance of a WorkBuddy resource response.
///
/// Exposed for per-account point queries (`workbuddy_auth`), which need the
/// same "summary fields at account level, else sum the segments" aggregation
/// `fetch_credits` applies to the wallet total — duplicating it would let the
/// pool's per-account numbers drift from the overview's total.
pub fn remaining_from_response(value: &Value) -> f64 {
    let accounts = collect_accounts(value);
    let mut sum = 0.0_f64;
    for account in &accounts {
        if let Some(r) = first_number(account, SUMMARY_REMAINING_FIELDS) {
            sum += r;
        }
    }
    if sum > 0.0 {
        return sum;
    }
    merge_segments(parse_workbuddy(value))
        .iter()
        .map(|s| s.remaining)
        .sum()
}

pub(crate) fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn first_number(item: &Value, fields: &[&str]) -> Option<f64> {
    for f in fields {
        let raw = match item.get(*f) {
            Some(v) => v,
            None => continue,
        };
        if raw.is_null() {
            continue;
        }
        if let Some(n) = raw.as_f64() {
            if n > 0.0 {
                return Some(n);
            }
        } else if let Some(s) = raw.as_str() {
            if let Ok(n) = s.trim().parse::<f64>() {
                if n > 0.0 {
                    return Some(n);
                }
            }
        }
    }
    None
}

pub(crate) fn parse_ts(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => {
            let n = n.as_f64()?;
            if n >= 1e12 {
                Some(n / 1000.0)
            } else {
                Some(n)
            }
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<f64>() {
                return if n >= 1e12 { Some(n / 1000.0) } else { Some(n) };
            }
            // Try "YYYY-MM-DD HH:MM:SS"; otherwise give up.
            if s.len() >= 19 {
                parse_datetime(s)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub(crate) fn parse_datetime(s: &str) -> Option<f64> {
    let (date, time) = s.split_once(' ')?;
    let (y, rest) = date.split_once('-')?;
    let (mo, d) = rest.split_once('-')?;
    let (hh, rest) = time.split_once(':')?;
    let (mm, ss) = rest.split_once(':')?;
    let (y, mo, d) = (
        y.parse::<i64>().ok()?,
        mo.parse::<u32>().ok()?,
        d.parse::<u32>().ok()?,
    );
    let (hh, mm, ss) = (
        hh.parse::<i64>().ok()?,
        mm.parse::<i64>().ok()?,
        ss.parse::<i64>().ok()?,
    );
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    Some(secs as f64)
}

pub(crate) fn days_from_civil(y: i64, mo: u32, d: u32) -> i64 {
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (367 * mo as u64 - 362) / 12 + (if mo <= 2 { 0 } else { 1 }) as u64 + d as u64
        - (if (mo == 2 && ((y % 4 == 0 && y % 100 != 0) || y % 400 == 0)) && d >= 29 {
            1
        } else {
            0
        });
    let moe = (yoe * 365 + yoe / 4 - yoe / 100) as i64 + doy as i64;
    era * 146097 + moe - 719468
}

pub(crate) fn first_text(item: &Value, fields: &[&str]) -> String {
    for f in fields {
        if let Some(s) = item.get(f).and_then(|v| v.as_str()) {
            let s = s.trim();
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Walk `value`, yielding every object that looks like a credit package.
pub(crate) fn collect_accounts(value: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    // WorkBuddy wrapped accounts.
    if let Some(accounts) = value
        .pointer("/data/Response/Data/Accounts")
        .or_else(|| value.pointer("/data/data/Response/Data/Accounts"))
        .or_else(|| value.pointer("/data/accounts"))
        .and_then(|v| v.as_array())
    {
        out.extend(accounts.iter().cloned());
    }
    // Generic `accounts` array at the top level.
    if let Some(arr) = value.get("accounts").and_then(|v| v.as_array()) {
        out.extend(arr.iter().cloned());
    }
    out
}

/// Extract a single positive credit number from a generic (unknown) payload.
///
/// Tries explicit `credits`/`balance`/`total` fields, then any numeric value
/// plausibly in the [0, 1e12) range. Returns `(credits, label)`.
pub(crate) fn extract_generic_credits(value: &Value) -> (f64, String) {
    for key in [
        "credits",
        "balance",
        "total",
        "remain",
        "remaining",
        "amount",
    ] {
        if let Some(n) = value.get(key).and_then(|v| v.as_f64()) {
            if n > 0.0 {
                return (n, key.to_string());
            }
        }
    }
    // Fall back to the largest positive number we can find one level deep.
    let mut best: Option<(f64, String)> = None;
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            if let Some(n) = v.as_f64() {
                if n > 0.0 && n < 1e12 {
                    let replace = match &best {
                        Some((b, _)) => n > *b,
                        None => true,
                    };
                    if replace {
                        best = Some((n, k.clone()));
                    }
                }
            }
        }
    }
    best.unwrap_or((0.0, String::new()))
}

/// Parse a WorkBuddy-style balance payload into segments.
pub(crate) fn parse_workbuddy(value: &Value) -> Vec<CreditSegment> {
    let mut segments = Vec::new();
    for account in collect_accounts(value) {
        // Expand slice-level usage details when present.
        let details = account
            .get("SlicePeriodUsageDetails")
            .and_then(|v| v.as_array());
        let items: Vec<Value> = if let Some(details) = details {
            details
                .iter()
                .filter(|d| d.is_object())
                .map(|d| {
                    let mut merged = account.clone();
                    if let Some(obj) = merged.as_object_mut() {
                        if let Some(d_obj) = d.as_object() {
                            for (k, v) in d_obj {
                                obj.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    merged
                })
                .collect()
        } else {
            vec![account]
        };

        for item in items {
            let remaining = match first_number(&item, REMAINING_FIELDS) {
                Some(r) => r,
                None => continue,
            };
            let total = first_number(&item, TOTAL_FIELDS).unwrap_or(remaining);
            let expires_at = EXPIRY_FIELDS
                .iter()
                .find_map(|f| item.get(f).and_then(parse_ts))
                .filter(|ts| *ts > now_epoch());
            let label = first_text(&item, LABEL_FIELDS);
            segments.push(CreditSegment {
                source: label,
                total: total.max(remaining),
                remaining,
                expires_at,
            });
        }
    }
    segments
}

pub(crate) fn merge_segments(mut segments: Vec<CreditSegment>) -> Vec<CreditSegment> {
    // Merge by (label, expires_at) so duplicate packages aggregate. We avoid
    // BTreeMap because `Option<f64>` is not `Ord` (f64 lacks total ordering).
    let mut merged: Vec<CreditSegment> = Vec::new();
    for s in segments.drain(..) {
        if s.remaining <= 0.0 {
            continue;
        }
        let key = (s.source.clone(), s.expires_at);
        if let Some(existing) = merged
            .iter_mut()
            .find(|e| (e.source.clone(), e.expires_at) == key)
        {
            existing.remaining += s.remaining;
            existing.total += s.total;
        } else {
            merged.push(s);
        }
    }
    let mut result = merged;
    for s in &mut result {
        s.remaining = (s.remaining * 100.0).round() / 100.0;
        s.total = (s.total * 100.0).round() / 100.0;
    }
    // Unknown expiries sort last.
    result.sort_by(|a, b| match (a.expires_at, b.expires_at) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
    });
    result
}

pub(crate) fn soonest_expiry(segments: &[CreditSegment]) -> Option<f64> {
    let now = now_epoch();
    segments
        .iter()
        .filter_map(|s| s.expires_at)
        .filter(|ts| *ts > now)
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}
