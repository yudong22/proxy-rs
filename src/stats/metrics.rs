//! Session metric aggregation and day statistics.

use super::*;

/// One completed upstream turn within a session, as the speed card needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub id: i64,
    pub created_at: String,
    pub model: String,
    /// Prompt tokens billed as fresh input.
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Prompt tokens served from the upstream's cache.
    pub cache_read_tokens: i64,
    /// End-to-end request duration (includes the client's read tail).
    pub duration_ms: i64,
    pub ttft_ms: i64,
    /// The model's own generation span (send → last output delta).
    pub model_ms: i64,
    pub ended_with_tool_call: bool,
}

impl SessionTurn {
    /// Whether this turn contributed a real generation span.
    ///
    /// An unmeasured row (a plain non-streamed reply, or one served before
    /// timing existed) has `model_ms == 0` and must not be counted as a zero-
    /// length generation — that would drag the session's speed to zero.
    pub fn is_measured(&self) -> bool {
        self.model_ms > 0
    }

    /// Output speed over the model's generation span. `None` when unmeasurable.
    pub fn tps(&self) -> Option<f64> {
        if self.output_tokens <= 0 || self.model_ms <= 0 {
            return None;
        }
        Some(self.output_tokens as f64 * 1000.0 / self.model_ms as f64)
    }
}

/// Hard cap on how many recent turns one session aggregate folds in.
///
/// The aggregate is a cumulative rate, so it is deliberately windowed rather
/// than unbounded: an all-time figure would stop responding to the session
/// getting faster or slower, while a single-turn figure jitters wildly. 3000
/// turns is far beyond any real conversation's useful memory while keeping the
/// query's work bounded.
pub const SESSION_METRICS_MAX_TURNS: i64 = 3000;

/// Session-level speed metrics, backing the overview's 输出速度 card.
///
/// **Aggregated over the session's recent turns, not the latest one.** A single
/// turn's speed swings by an order of magnitude (a short reply spends most of
/// its span in the first-token gap), so the card used to change on every poll.
/// Aggregating fixes that the correct way for a *rate*: sum the output tokens and
/// sum the generation time, then divide once.
///
/// ```text
///   tps = Σ output_tokens / Σ model_ms
/// ```
///
/// That is a time-weighted mean. Averaging the per-turn `tok/s` values instead
/// would be wrong: it would let a 3-token reply and a 3000-token reply count
/// equally.
/// All-time totals for one conversation, as seen by the gateway.
///
/// Joined to a local transcript by its namespaced id (`claude:<uuid>`). Every
/// field is a sum over every recorded request, never a window: this answers
/// "what did this conversation cost through the proxy", not "how fast is it
/// right now" (that is [`SessionMetrics`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionUsageTotal {
    pub session_id: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    /// Distinct served models, sorted for a stable payload.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub session_id: String,
    /// Turns folded in (≤ [`SESSION_METRICS_MAX_TURNS`]).
    pub turns: i64,
    /// How many of those carried a real generation span.
    pub measured_turns: i64,
    /// Model of the most recent turn, for the panel.
    pub model: Option<String>,
    /// When the most recent turn finished.
    pub last_at: Option<String>,
    /// Σ output tokens, over measured turns only.
    pub output_tokens: i64,
    /// Σ model generation span (ms), over measured turns only.
    pub model_ms: i64,
    /// Σ measured TTFTs (ms) and how many were measured, for the average.
    pub ttft_sum_ms: i64,
    pub ttft_samples: i64,
    /// Σ tool wait (ms) attributed within this window, and how many gaps.
    pub tool_wait_ms: i64,
    pub tool_waits: i64,
    /// Σ fresh input tokens across the window (all turns, measured or not —
    /// this is a token count, not a rate, so an unmeasured turn still counts).
    pub input_tokens: i64,
    /// Σ prompt tokens served from cache across the same window.
    pub cache_read_tokens: i64,
    /// How many subagent child sessions were folded into this figure.
    ///
    /// Zero means either "this conversation spawned none" or "none of them were
    /// recorded after lineage reporting existed". Those are not the same claim,
    /// but the gateway cannot tell them apart: pre-lineage rows carry an empty
    /// parent id and there is nothing to backfill from. The UI must therefore
    /// present this as a count, never as "no subagents ran".
    pub subagent_sessions: i64,
    /// Whether this row *is* a subagent child rather than a top-level
    /// conversation.
    ///
    /// Set from `parent_session_id`, so it is false for both a real top-level
    /// conversation and every historical row.
    pub is_subagent: bool,
}

impl SessionMetrics {
    /// Session output speed: total tokens over total generation time.
    /// `None` when nothing in the window was measurable.
    pub fn tps(&self) -> Option<f64> {
        if self.output_tokens <= 0 || self.model_ms <= 0 {
            return None;
        }
        Some(self.output_tokens as f64 * 1000.0 / self.model_ms as f64)
    }

    /// Add one child session's measurements into this aggregate.
    ///
    /// Sums only — every field here is already a total or a sample count, so
    /// adding two sessions' figures yields the combined session's figure. The
    /// derived values ([`Self::tps`], [`Self::avg_ttft_ms`]) are computed after
    /// the fact from these sums, which is what makes the combined rate a true
    /// tokens-over-time rather than an average of two rates.
    ///
    /// `last_at`/`model`/`session_id` are *not* taken from the child: they
    /// describe the conversation this row is labelled with, and letting a child
    /// overwrite them would relabel a parent's column with a subagent's model.
    pub(crate) fn absorb(&mut self, child: &SessionMetrics) {
        self.turns += child.turns;
        self.measured_turns += child.measured_turns;
        self.output_tokens += child.output_tokens;
        self.model_ms += child.model_ms;
        self.ttft_sum_ms += child.ttft_sum_ms;
        self.ttft_samples += child.ttft_samples;
        self.tool_wait_ms += child.tool_wait_ms;
        self.tool_waits += child.tool_waits;
        self.input_tokens += child.input_tokens;
        self.cache_read_tokens += child.cache_read_tokens;
    }

    /// Mean TTFT across the measured turns, or `None` when none was measured.
    pub fn avg_ttft_ms(&self) -> Option<i64> {
        if self.ttft_samples <= 0 {
            return None;
        }
        Some(self.ttft_sum_ms / self.ttft_samples)
    }

    /// Total tool time attributed in the window, or `None` when no tool gap was
    /// observable (so the panel shows "—" rather than a bare 0).
    pub fn tool_wait_ms(&self) -> Option<i64> {
        if self.tool_waits <= 0 {
            return None;
        }
        Some(self.tool_wait_ms)
    }

    /// Cache-hit percentage over the window: cache_read / (input + cache_read).
    ///
    /// Same formula as [`DayStats::cache_hit_pct`], so a session's figure and
    /// the overview's daily figure mean the same thing. `None` when the window
    /// carried no prompt traffic at all: a session with no requests has no hit
    /// rate, and showing "0%" would read as "nothing was cached" rather than
    /// "nothing was measured" — the same distinction the speed card draws
    /// between a real 0 and an unmeasured value.
    pub fn cache_hit_pct(&self) -> Option<i64> {
        let denom = self.input_tokens + self.cache_read_tokens;
        if denom <= 0 {
            return None;
        }
        Some(self.cache_read_tokens * 100 / denom)
    }
}

/// Fold a session's turns (oldest first) into one aggregate.
///
/// Pure and separate from the query so the weighting rules are unit-testable
/// without a database.
///
/// Tool time is accumulated only where a tool-requesting turn is immediately
/// followed by another turn in the same session — see [`tool_wait_ms`] for why
/// the arithmetic subtracts the follower's own duration, and why an implausible
/// or negative gap counts as no measurement rather than as zero.
pub(crate) fn fold_session_turns(session_id: &str, turns: &[SessionTurn]) -> SessionMetrics {
    let mut metrics = SessionMetrics {
        session_id: session_id.to_string(),
        turns: turns.len() as i64,
        ..Default::default()
    };

    let mut previous: Option<&SessionTurn> = None;
    for turn in turns {
        if turn.is_measured() {
            metrics.measured_turns += 1;
            metrics.output_tokens += turn.output_tokens;
            metrics.model_ms += turn.model_ms;
        }
        // Token totals count every turn: they are quantities, not rates, so a
        // turn with no timing still contributed prompt traffic.
        metrics.input_tokens += turn.input_tokens;
        metrics.cache_read_tokens += turn.cache_read_tokens;
        if turn.ttft_ms > 0 {
            metrics.ttft_samples += 1;
            metrics.ttft_sum_ms += turn.ttft_ms;
        }
        if let Some(prev) = previous {
            if prev.ended_with_tool_call {
                if let Some(gap) =
                    tool_wait_ms(&prev.created_at, &turn.created_at, turn.duration_ms)
                {
                    metrics.tool_wait_ms += gap;
                    metrics.tool_waits += 1;
                }
            }
        }
        previous = Some(turn);
    }

    if let Some(last) = turns.last() {
        metrics.model = Some(last.model.clone());
        metrics.last_at = Some(last.created_at.clone());
    }

    metrics
}

/// Milliseconds the client spent running tools, derived from the gap between a
/// tool-requesting turn and its successor.
///
/// Both `created_at` values are stamped when the row is *written* — i.e. at the
/// **end** of each request, not its start (verified against the running app: a
/// request that took 1.3 s was stamped as it completed). So the raw difference
/// between the two stamps is *not* the tool wait; it also contains the current
/// turn's own duration. Subtracting that duration backs out the moment this turn
/// began, which is what puts the gap on the client:
///
/// ```text
///   tool wait = (cur_end - prev_end) - cur_duration
///             = cur_start - prev_end
/// ```
///
/// Timestamps are stored at second granularity, so the result is accurate to
/// about a second — fine for a tool that takes seconds to minutes, and the UI
/// shows one decimal.
///
/// Returns `None` when either timestamp is unparseable or the result is negative
/// or implausibly large: a session resumed hours later must not present as hours
/// of tool execution, which would be a worse lie than showing nothing.
pub(crate) fn tool_wait_ms(prev_at: &str, cur_at: &str, cur_duration_ms: i64) -> Option<i64> {
    /// Above this, a "tool call" is really a paused/resumed conversation.
    const MAX_PLAUSIBLE_TOOL_WAIT_MS: i64 = 10 * 60 * 1000;

    let prev_end = parse_local_datetime_secs(prev_at)?;
    let cur_end = parse_local_datetime_secs(cur_at)?;
    let wait = (cur_end - prev_end) * 1000 - cur_duration_ms.max(0);
    if !(0..=MAX_PLAUSIBLE_TOOL_WAIT_MS).contains(&wait) {
        return None;
    }
    Some(wait)
}

/// Parse the stats DB's local `YYYY-MM-DD HH:MM:SS` stamp into epoch seconds.
///
/// Deliberately hand-rolled: the column is written by this module in one fixed
/// local-time format, and pulling in a date-time crate (or re-deriving the UTC
/// offset, which can have changed between the two rows across a DST boundary)
/// would add a dependency for one subtraction. Two timestamps in the same local
/// format differ by the same amount as the instants they denote.
pub(crate) fn parse_local_datetime_secs(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let min: i64 = value.get(14..16)?.parse().ok()?;
    let sec: i64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Days since the Unix epoch, proleptic Gregorian — enough for the 1970+ and
    // 2100- ranges this table can hold.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Aggregated statistics for a single calendar day.
/// Derived on demand from `request_logs` — there is no separate daily table, so
/// the counters can never drift out of sync with the rows they summarize.
#[derive(Debug, Clone, Default)]
pub struct DayStats {
    pub date: String,
    pub requests_total: i64,
    pub requests_success: i64,
    pub requests_failed: i64,
    pub tokens_input: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub tokens_output: i64,
    /// Requests whose credential/model was changed by an exception override
    /// (failover, degraded retry, token refresh). Empty `override_key` rows do
    /// not count: a request served exactly as configured is not an override.
    pub overrides_total: i64,
}

impl DayStats {
    /// Total tokens consumed (all input categories + output).
    pub fn tokens_total(&self) -> i64 {
        self.tokens_input + self.tokens_cache_read + self.tokens_cache_write + self.tokens_output
    }

    /// Cache-hit percentage: cache_read / (input + cache_read) × 100.
    /// Returns 0 when there is no input traffic.
    pub fn cache_hit_pct(&self) -> i64 {
        let denom = self.tokens_input + self.tokens_cache_read;
        if denom == 0 {
            0
        } else {
            self.tokens_cache_read * 100 / denom
        }
    }
}
