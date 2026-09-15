//! Show-operations bounded context.
//!
//! Only tasks backed by verifiable first-party facts may be auto-completed.
//! Physical checks remain human work and are escalated, never guessed.

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::{EventId, autonomy::Confidence};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowTaskKind {
    AnnouncementPublished,
    TicketingVerified,
    StaffAssigned,
    OfflineSnapshotReady,
    GateDeviceCharged,
    BackupDeviceReady,
    NetworkTested,
    GuestlistChecked,
    CapturePlan,
    PostShowReconciliation,
    PostShowReport,
}

impl ShowTaskKind {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::AnnouncementPublished => "announcement_published",
            Self::TicketingVerified => "ticketing_verified",
            Self::StaffAssigned => "staff_assigned",
            Self::OfflineSnapshotReady => "offline_snapshot_ready",
            Self::GateDeviceCharged => "gate_device_charged",
            Self::BackupDeviceReady => "backup_device_ready",
            Self::NetworkTested => "network_tested",
            Self::GuestlistChecked => "guestlist_checked",
            Self::CapturePlan => "capture_plan",
            Self::PostShowReconciliation => "post_show_reconciliation",
            Self::PostShowReport => "post_show_report",
        }
    }

    #[must_use]
    pub const fn is_physical(self) -> bool {
        matches!(
            self,
            Self::GateDeviceCharged
                | Self::BackupDeviceReady
                | Self::NetworkTested
                | Self::CapturePlan
        )
    }

    #[must_use]
    const fn is_post_show(self) -> bool {
        matches!(self, Self::PostShowReconciliation | Self::PostShowReport)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShowTaskSnapshot {
    pub event_id: EventId,
    pub task: ShowTaskKind,
    pub starts_at: OffsetDateTime,
    pub already_done: bool,
    pub verifiable_fact: bool,
    pub last_escalated_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShowOperationsPolicy {
    pub escalate_hours_before: u32,
    pub post_show_escalate_hours: u32,
    /// Hours after the show when the T+7 report is due. The report is
    /// system-generated — first-party numbers honestly labelled, delivered to
    /// band and counterparty — so it waits a week for receipts (campaign
    /// deliveries, harvest artifacts) to land before it speaks.
    #[serde(default = "default_post_show_report_hours")]
    pub post_show_report_hours: u32,
    pub escalation_cooldown_hours: u32,
}

const fn default_post_show_report_hours() -> u32 {
    168
}

/// The report horizon cannot outgrow the snapshot that evaluates it.
///
/// The loader trails shows for 9 days, so a configured due past ~8 days
/// would come due only after the show had already aged out — an artifact
/// that can never ship. 192 hours leaves a day of evaluation and retry
/// room inside the window.
pub const MAX_POST_SHOW_REPORT_HOURS: u32 = 192;

impl ShowOperationsPolicy {
    /// Rejects horizons the evaluator cannot honor. Runs on every policy
    /// parse — write and read alike — so an out-of-range stored row fails
    /// loudly instead of silently suppressing the one report that exists.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.post_show_report_hours > MAX_POST_SHOW_REPORT_HOURS {
            return Err("post_show_report_hours exceeds the evaluable window");
        }
        Ok(())
    }
}

impl Default for ShowOperationsPolicy {
    fn default() -> Self {
        Self {
            escalate_hours_before: 36,
            post_show_escalate_hours: 12,
            post_show_report_hours: default_post_show_report_hours(),
            escalation_cooldown_hours: 12,
        }
    }
}

/// How many times the escalation interval may double.
///
/// Four doublings takes a 12-hour cooldown to eight days, which is far enough
/// apart to stop being noise and close enough to stay a reminder. Capped
/// rather than unbounded because a task nobody will ever complete should still
/// resurface occasionally -- silence would be a different kind of wrong.
const MAX_ESCALATION_DOUBLINGS: u32 = 4;

/// The gap before an unanswered escalation may repeat.
///
/// The interval was a constant, so a task ignored for six days was escalated
/// at exactly the rate of one ignored for six hours. Production shows what
/// that costs: one show task escalated 61 times across six days, 73 of 551
/// decisions in the whole ledger spent re-asking two subjects the same
/// question. Nobody reads the twelfth reminder more carefully than the first.
///
/// So the interval widens with how long the task has been due -- the only
/// evidence available here, and it needs no escalation counter and no schema
/// change. Each full base period doubles the gap, up to
/// [`MAX_ESCALATION_DOUBLINGS`]: 12h, 24h, 48h, 96h, then 8 days.
#[must_use]
pub fn escalation_interval(policy: ShowOperationsPolicy, overdue: Duration) -> Duration {
    let base_hours = i64::from(policy.escalation_cooldown_hours.max(1));
    let base = Duration::hours(base_hours);
    // Whole base periods elapsed since the task fell due. Saturating: a clock
    // skew that makes `overdue` negative is not a reason to escalate faster.
    let periods = overdue
        .whole_hours()
        .max(0)
        .checked_div(base_hours)
        .unwrap_or(0);
    let doublings = u32::try_from(periods)
        .unwrap_or(MAX_ESCALATION_DOUBLINGS)
        .min(MAX_ESCALATION_DOUBLINGS);
    base * 2_i32.saturating_pow(doublings)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowOperationsDecision {
    Hold(ShowOperationsHoldReason),
    AutoComplete { confidence: Confidence },
    EscalateHuman { confidence: Confidence },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShowOperationsHoldReason {
    AlreadyDone,
    NotDue,
    Cooldown,
}

#[must_use]
pub fn evaluate_show_task(
    snapshot: ShowTaskSnapshot,
    policy: ShowOperationsPolicy,
    now: OffsetDateTime,
) -> ShowOperationsDecision {
    if snapshot.already_done {
        return ShowOperationsDecision::Hold(ShowOperationsHoldReason::AlreadyDone);
    }

    if !snapshot.task.is_physical() && snapshot.verifiable_fact {
        return ShowOperationsDecision::AutoComplete {
            confidence: Confidence::saturating_from_basis_points(10_000),
        };
    }

    let due_at = match snapshot.task {
        // The report is the system's own artifact, not a nag: it goes out once
        // a week of receipts has had time to land.
        ShowTaskKind::PostShowReport => {
            snapshot.starts_at + Duration::hours(i64::from(policy.post_show_report_hours))
        }
        task if task.is_post_show() => {
            snapshot.starts_at + Duration::hours(i64::from(policy.post_show_escalate_hours))
        }
        _ => snapshot.starts_at - Duration::hours(i64::from(policy.escalate_hours_before)),
    };
    if now < due_at {
        return ShowOperationsDecision::Hold(ShowOperationsHoldReason::NotDue);
    }

    let cooldown = escalation_interval(policy, now - due_at);
    if snapshot
        .last_escalated_at
        .is_some_and(|at| at <= now && now - at < cooldown)
    {
        return ShowOperationsDecision::Hold(ShowOperationsHoldReason::Cooldown);
    }

    ShowOperationsDecision::EscalateHuman {
        confidence: Confidence::saturating_from_basis_points(10_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000)
    }

    #[test]
    fn database_evidence_can_complete_non_physical_task() {
        let snapshot = ShowTaskSnapshot {
            event_id: EventId::new(),
            task: ShowTaskKind::TicketingVerified,
            starts_at: now() + Duration::days(3),
            already_done: false,
            verifiable_fact: true,
            last_escalated_at: None,
        };
        assert!(matches!(
            evaluate_show_task(snapshot, ShowOperationsPolicy::default(), now()),
            ShowOperationsDecision::AutoComplete { .. }
        ));
    }

    /// The reminder cadence that made 61 escalations of one task in six days.
    #[test]
    fn the_escalation_gap_widens_the_longer_a_task_is_ignored() {
        let policy = ShowOperationsPolicy::default();
        let base = Duration::hours(12);
        assert_eq!(escalation_interval(policy, Duration::ZERO), base);
        assert_eq!(escalation_interval(policy, Duration::hours(12)), base * 2);
        assert_eq!(escalation_interval(policy, Duration::hours(24)), base * 4);
        assert_eq!(escalation_interval(policy, Duration::hours(36)), base * 8);
    }

    #[test]
    fn the_escalation_gap_stops_widening_so_a_task_never_goes_silent() {
        let policy = ShowOperationsPolicy::default();
        let capped = Duration::hours(12) * 2_i32.pow(MAX_ESCALATION_DOUBLINGS);
        assert_eq!(escalation_interval(policy, Duration::days(30)), capped);
        assert_eq!(escalation_interval(policy, Duration::days(365)), capped);
    }

    /// A clock that runs backwards must not escalate faster than a clock that
    /// does not.
    #[test]
    fn a_negative_overdue_uses_the_base_interval() {
        let policy = ShowOperationsPolicy::default();
        assert_eq!(
            escalation_interval(policy, Duration::hours(-48)),
            Duration::hours(12)
        );
    }

    #[test]
    fn a_long_ignored_task_is_held_where_a_fresh_one_would_escalate() {
        let policy = ShowOperationsPolicy::default();
        // Due four days ago, last escalated a day ago. Under the old fixed
        // 12-hour cooldown this escalated again; now the gap is 8 days.
        let snapshot = ShowTaskSnapshot {
            event_id: EventId::new(),
            task: ShowTaskKind::GateDeviceCharged,
            starts_at: now() - Duration::days(4) + Duration::hours(36),
            already_done: false,
            verifiable_fact: false,
            last_escalated_at: Some(now() - Duration::days(1)),
        };
        assert_eq!(
            evaluate_show_task(snapshot, policy, now()),
            ShowOperationsDecision::Hold(ShowOperationsHoldReason::Cooldown)
        );
    }

    /// The report waits a week for receipts while reconciliation still fires
    /// the night after — same family, different clocks.
    #[test]
    fn the_report_is_due_a_week_after_the_show_not_the_next_morning() {
        let policy = ShowOperationsPolicy::default();
        let starts_at = now() - Duration::days(3);
        let report = ShowTaskSnapshot {
            event_id: EventId::new(),
            task: ShowTaskKind::PostShowReport,
            starts_at,
            already_done: false,
            verifiable_fact: false,
            last_escalated_at: None,
        };
        assert_eq!(
            evaluate_show_task(report, policy, now()),
            ShowOperationsDecision::Hold(ShowOperationsHoldReason::NotDue)
        );
        let reconciliation = ShowTaskSnapshot {
            task: ShowTaskKind::PostShowReconciliation,
            ..report
        };
        assert!(matches!(
            evaluate_show_task(reconciliation, policy, now()),
            ShowOperationsDecision::EscalateHuman { .. }
        ));
    }

    #[test]
    fn the_report_escalates_once_its_week_has_passed() {
        let report = ShowTaskSnapshot {
            event_id: EventId::new(),
            task: ShowTaskKind::PostShowReport,
            starts_at: now() - Duration::days(7) - Duration::hours(1),
            already_done: false,
            verifiable_fact: false,
            last_escalated_at: None,
        };
        assert!(matches!(
            evaluate_show_task(report, ShowOperationsPolicy::default(), now()),
            ShowOperationsDecision::EscalateHuman { .. }
        ));
    }

    /// A horizon past the snapshot's trailing edge can never be evaluated —
    /// the show ages out before its report comes due — so it is rejected
    /// rather than silently suppressing the artifact.
    #[test]
    fn a_report_horizon_beyond_the_evaluable_window_is_rejected() {
        let policy = ShowOperationsPolicy {
            post_show_report_hours: MAX_POST_SHOW_REPORT_HOURS + 1,
            ..ShowOperationsPolicy::default()
        };
        assert!(policy.validate().is_err());
        assert!(ShowOperationsPolicy::default().validate().is_ok());
        assert!(
            ShowOperationsPolicy {
                post_show_report_hours: MAX_POST_SHOW_REPORT_HOURS,
                ..ShowOperationsPolicy::default()
            }
            .validate()
            .is_ok()
        );
    }

    /// Policies persisted before the field existed must still parse — the
    /// missing key falls back to the one-week default rather than failing the
    /// whole policy read.
    #[test]
    fn a_stored_policy_without_the_report_window_keeps_its_other_values() {
        let parsed: ShowOperationsPolicy = serde_json::from_value(serde_json::json!({
            "escalate_hours_before": 24,
            "post_show_escalate_hours": 8,
            "escalation_cooldown_hours": 6
        }))
        .expect("legacy policy shape parses");
        assert_eq!(parsed.escalate_hours_before, 24);
        assert_eq!(parsed.post_show_escalate_hours, 8);
        assert_eq!(parsed.escalation_cooldown_hours, 6);
        assert_eq!(parsed.post_show_report_hours, 168);
    }

    #[test]
    fn physical_task_is_never_auto_completed() {
        let snapshot = ShowTaskSnapshot {
            event_id: EventId::new(),
            task: ShowTaskKind::GateDeviceCharged,
            starts_at: now() + Duration::hours(12),
            already_done: false,
            verifiable_fact: true,
            last_escalated_at: None,
        };
        assert!(matches!(
            evaluate_show_task(snapshot, ShowOperationsPolicy::default(), now()),
            ShowOperationsDecision::EscalateHuman { .. }
        ));
    }
}
