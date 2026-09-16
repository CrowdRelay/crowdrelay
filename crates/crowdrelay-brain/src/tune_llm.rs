//! Tune LLM — the brain adjusts worker call parameters from call telemetry.
//!
//! The agent service writes one row per LLM call to `agent_service_llm_calls`.
//! This module reads the recent tail and emits a tuning decision the runner
//! resolves before each call: a temperature override, a `max_tokens` scale,
//! and a paid breaker deadline. Three rules, each reading a different
//! failure signature:
//!
//! - **Paid breaker.** Sustained paid failure — three consecutive paid
//!   failures at the tail, or a paid error rate at or above half over a
//!   window of at least ten — opens the breaker for `BREAKER_COOLDOWN`.
//!   While it is open the runner skips paid models; free ones keep working.
//!   The breaker is the fail-closed bound on spend: a dying provider must
//!   not keep burning. A live breaker is preserved across evaluations until
//!   its own expiry — thin evidence cannot shorten an earned cooldown.
//! - **Truncation scale.** When at least a fifth of the tail ended on a
//!   length stop (`finish_reason` = `length`/`max_tokens`), the request
//!   budget is too small for the task — `max_tokens_scale` rises by half,
//!   capped at [`SCALE_CEILING`]. When the signature clears, the scale does:
//!   `None` hands the default back.
//! - **Unclassified temperature.** Structured-outcome calls report
//!   `classified`. When at least two fifths of the classified-eligible tail
//!   failed to parse into the declared schema, the model is wandering —
//!   temperature drops to [`TEMP_LOW`]. At a healthy rate the override
//!   clears and the runner returns to its default.
//!
//! What this never does: pick a model (the operator owns the chain), touch
//! prompts, loosen consent or budget caps, or claim better content — it
//! tunes *reliability*. Whether lower temperature produces better outcomes
//! is a treatment-effect question and its sample rules live there.
//!
//! Every rule has a minimum evidence floor: under `MIN_WINDOW` calls the
//! tail cannot support a claim, and `evaluate` returns defaults. A missing
//! number is `None`, never `0` — the tuning row only carries what evidence
//! produced.

use time::OffsetDateTime;

/// One telemetry row as the rule needs it — the runner's record of a call.
#[derive(Debug, Clone, Copy)]
pub struct LlmCallTail<'a> {
    pub ok: bool,
    /// `Some` only for structured-outcome tasks: the response parsed into
    /// the declared schema. Free-form calls do not enter the unclassified
    /// rate at all.
    pub classified: Option<bool>,
    pub paid: bool,
    /// Provider stop reason when the response was cut — `length` or
    /// `max_tokens` marks truncation.
    pub finish_reason: Option<&'a str>,
    pub created_at: OffsetDateTime,
}

/// The decision the runner resolves. `None` fields mean "use the default" —
/// the runner never reads a tuned value the evidence did not produce.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmTuning {
    pub temperature: Option<f64>,
    pub max_tokens_scale: Option<f64>,
    pub paid_breaker_until: Option<OffsetDateTime>,
    /// One line for the tuning row's `reason` column — the operator should be
    /// able to read why the brain changed the knobs without replaying the
    /// tail.
    pub reason: String,
}

impl LlmTuning {
    /// Evidence produced nothing — every field defaults.
    pub fn defaults() -> Self {
        Self {
            temperature: None,
            max_tokens_scale: None,
            paid_breaker_until: None,
            reason: String::new(),
        }
    }
}

/// Below this many calls in the tail, no rule may fire.
const MIN_WINDOW: usize = 10;
/// Paid failure share that opens the breaker over a >= MIN_WINDOW window.
const PAID_ERROR_RATE: f64 = 0.5;
/// Consecutive paid failures at the tail that open the breaker regardless.
const PAID_FAIL_STREAK: usize = 3;
/// How long the breaker stays open once tripped.
pub const BREAKER_COOLDOWN: time::Duration = time::Duration::minutes(30);
/// Length-stop share that raises the max_tokens scale.
const TRUNCATION_RATE: f64 = 0.2;
/// Steps the scale can take: each firing adds half again the base budget.
pub const SCALE_STEP: f64 = 1.5;
/// The scale may never grow past this — a truncation loop must not inflate
/// the request budget without bound.
pub const SCALE_CEILING: f64 = 2.0;
/// Unclassified share that drops the temperature.
const UNCLASSIFIED_RATE: f64 = 0.4;
/// The temperature the unclassified rule lands on.
pub const TEMP_LOW: f64 = 0.3;
/// The hard bounds the runner clamps to as well — a tuning value outside
/// this range is a bug, not a knob.
pub const TEMP_FLOOR: f64 = 0.1;
pub const TEMP_CEILING: f64 = 1.0;

fn is_truncation(finish_reason: Option<&str>) -> bool {
    matches!(finish_reason, Some("length") | Some("max_tokens"))
}

/// Read the tail (most recent first, as the index serves it) and emit the
/// tuning decision. `previous_scale` is the scale currently in effect —
/// truncation raises from it toward the ceiling rather than jumping.
/// `previous_breaker_until` is the breaker already in force: a live one is
/// carried forward until its own expiry instead of being cleared by a tail
/// too thin to judge.
pub fn evaluate(
    calls: &[LlmCallTail],
    now: OffsetDateTime,
    previous_scale: Option<f64>,
    previous_breaker_until: Option<OffsetDateTime>,
) -> LlmTuning {
    if calls.len() < MIN_WINDOW {
        let mut tuning = LlmTuning::defaults();
        if previous_breaker_until.is_some_and(|until| until > now) {
            // Same rule as below: the cooldown is time-bound and does not
            // need continuous evidence to persist. A quiet stretch produces
            // a thin tail, and a thin tail must not free a breaker that has
            // not yet expired.
            tuning.paid_breaker_until = previous_breaker_until;
        }
        return tuning;
    }

    let mut tuning = LlmTuning::defaults();
    let mut reasons: Vec<String> = Vec::new();

    // ── Paid breaker ─────────────────────────────────────────────────
    let mut streak = 0usize;
    for call in calls {
        if !call.paid {
            break; // the tail is newest-first; the streak is contiguous paid failures
        }
        if call.ok {
            break;
        }
        streak += 1;
        if streak >= PAID_FAIL_STREAK {
            break;
        }
    }
    let paid: Vec<&LlmCallTail> = calls.iter().filter(|c| c.paid).collect();
    let paid_error_rate = if paid.is_empty() {
        0.0
    } else {
        paid.iter().filter(|c| !c.ok).count() as f64 / paid.len() as f64
    };
    if streak >= PAID_FAIL_STREAK
        || (paid.len() >= MIN_WINDOW && paid_error_rate >= PAID_ERROR_RATE)
    {
        tuning.paid_breaker_until = Some(now + BREAKER_COOLDOWN);
        reasons.push(if streak >= PAID_FAIL_STREAK {
            format!("{streak} consecutive paid failures")
        } else {
            format!(
                "paid error rate {:.0}% over {} calls",
                paid_error_rate * 100.0,
                paid.len()
            )
        });
    } else if previous_breaker_until.is_some_and(|until| until > now) {
        // A live breaker is a time-bound embargo, not a continuous claim:
        // it expires on its own clock, and a stretch of thin evidence must
        // not shorten a cooldown that was earned by real failures.
        tuning.paid_breaker_until = previous_breaker_until;
    }

    // ── Truncation scale ─────────────────────────────────────────────
    let truncated = calls
        .iter()
        .filter(|c| is_truncation(c.finish_reason))
        .count();
    let truncation_rate = truncated as f64 / calls.len() as f64;
    if truncation_rate >= TRUNCATION_RATE {
        let next = (previous_scale.unwrap_or(1.0) * SCALE_STEP).min(SCALE_CEILING);
        if next > previous_scale.unwrap_or(1.0) {
            tuning.max_tokens_scale = Some(next);
            reasons.push(format!(
                "{truncated}/{} calls truncated — max_tokens scale {next:.2}",
                calls.len()
            ));
        } else {
            // Already at the ceiling; keep it so the runner does not drop
            // back to default mid-signature — and say so, because an active
            // restriction with an empty reason reads as a knob nobody set.
            tuning.max_tokens_scale = previous_scale;
            reasons.push(format!(
                "{truncated}/{} calls truncated — max_tokens scale held at ceiling {SCALE_CEILING}",
                calls.len()
            ));
        }
    }

    // ── Unclassified temperature ─────────────────────────────────────
    let structured: Vec<&LlmCallTail> = calls.iter().filter(|c| c.classified.is_some()).collect();
    if structured.len() >= MIN_WINDOW {
        let unclassified = structured
            .iter()
            .filter(|c| c.classified == Some(false))
            .count();
        let rate = unclassified as f64 / structured.len() as f64;
        if rate >= UNCLASSIFIED_RATE {
            tuning.temperature = Some(TEMP_LOW);
            reasons.push(format!(
                "{unclassified}/{} structured calls unclassified — temperature {TEMP_LOW}",
                structured.len()
            ));
        }
    }

    tuning.reason = reasons.join("; ");
    tuning
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap()
    }

    fn call(ok: bool, paid: bool) -> LlmCallTail<'static> {
        LlmCallTail {
            ok,
            classified: None,
            paid,
            finish_reason: None,
            created_at: now(),
        }
    }

    #[test]
    fn empty_tail_defaults() {
        assert_eq!(evaluate(&[], now(), None, None), LlmTuning::defaults());
    }

    #[test]
    fn below_window_defaults() {
        let calls = vec![call(false, true); MIN_WINDOW - 1];
        let t = evaluate(&calls, now(), None, None);
        assert_eq!(t, LlmTuning::defaults());
    }

    #[test]
    fn consecutive_paid_failures_open_breaker() {
        // newest-first: three paid failures at the head
        let mut calls = vec![call(false, true); 3];
        calls.extend(vec![call(true, true); 10]);
        let t = evaluate(&calls, now(), None, None);
        assert_eq!(t.paid_breaker_until, Some(now() + BREAKER_COOLDOWN));
        assert!(t.reason.contains("3 consecutive paid failures"));
    }

    #[test]
    fn paid_error_rate_opens_breaker() {
        // interleaved failures — no streak, but rate >= 50%
        let mut calls = Vec::new();
        for _ in 0..6 {
            calls.push(call(false, true));
            calls.push(call(true, true));
        }
        let t = evaluate(&calls, now(), None, None);
        assert!(t.paid_breaker_until.is_some());
        assert!(t.reason.contains("paid error rate"));
    }

    #[test]
    fn healthy_paid_stays_closed() {
        let calls = vec![call(true, true); 20];
        let t = evaluate(&calls, now(), None, None);
        assert!(t.paid_breaker_until.is_none());
    }

    #[test]
    fn truncation_raises_scale_from_previous() {
        let mut calls = vec![call(true, false); 10];
        for c in calls.iter_mut().take(3) {
            c.finish_reason = Some("length");
        }
        let t = evaluate(&calls, now(), Some(1.5), None);
        assert_eq!(t.max_tokens_scale, Some(2.0));
    }

    #[test]
    fn truncation_respects_ceiling() {
        let mut calls = vec![call(true, false); 10];
        for c in calls.iter_mut().take(4) {
            c.finish_reason = Some("max_tokens");
        }
        let t = evaluate(&calls, now(), Some(2.0), None);
        assert_eq!(t.max_tokens_scale, Some(2.0));
    }

    #[test]
    fn unclassified_drops_temperature() {
        let mut calls = vec![call(true, false); 10];
        for (i, c) in calls.iter_mut().enumerate() {
            c.classified = Some(i >= 6); // 4 of 10 unclassified
        }
        let t = evaluate(&calls, now(), None, None);
        assert_eq!(t.temperature, Some(TEMP_LOW));
    }

    #[test]
    fn freeform_calls_do_not_enter_unclassified_rate() {
        // 10 free-form (classified=None) + nothing else — no claim possible
        let calls = vec![call(true, false); 10];
        let t = evaluate(&calls, now(), None, None);
        assert_eq!(t, LlmTuning::defaults());
    }

    #[test]
    fn live_breaker_survives_thin_tail() {
        // A quiet stretch must not shorten an earned cooldown.
        let until = now() + BREAKER_COOLDOWN;
        let calls = vec![call(true, false); 3];
        let t = evaluate(&calls, now(), None, Some(until));
        assert_eq!(t.paid_breaker_until, Some(until));
        assert_eq!(t.temperature, None);
    }

    #[test]
    fn live_breaker_survives_healthy_tail() {
        let until = now() + BREAKER_COOLDOWN;
        let calls = vec![call(true, true); 20];
        let t = evaluate(&calls, now(), None, Some(until));
        assert_eq!(t.paid_breaker_until, Some(until));
    }

    #[test]
    fn expired_breaker_clears() {
        let until = now() - time::Duration::minutes(5);
        let calls = vec![call(true, true); 20];
        let t = evaluate(&calls, now(), None, Some(until));
        assert_eq!(t.paid_breaker_until, None);
    }

    #[test]
    fn low_unclassified_rate_clears_override() {
        let mut calls = vec![call(true, false); 10];
        calls[0].classified = Some(false);
        for c in calls.iter_mut().skip(1) {
            c.classified = Some(true);
        }
        let t = evaluate(&calls, now(), None, None);
        assert_eq!(t.temperature, None);
    }
}
