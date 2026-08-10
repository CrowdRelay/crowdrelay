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
            Self::PostShowReconciliation => "post_show_reconciliation",
            Self::PostShowReport => "post_show_report",
        }
    }

    #[must_use]
    pub const fn is_physical(self) -> bool {
        matches!(
            self,
            Self::GateDeviceCharged | Self::BackupDeviceReady | Self::NetworkTested
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
    pub escalation_cooldown_hours: u32,
}

impl Default for ShowOperationsPolicy {
    fn default() -> Self {
        Self {
            escalate_hours_before: 36,
            post_show_escalate_hours: 12,
            escalation_cooldown_hours: 12,
        }
    }
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

    let due = if snapshot.task.is_post_show() {
        now >= snapshot.starts_at + Duration::hours(i64::from(policy.post_show_escalate_hours))
    } else {
        now >= snapshot.starts_at - Duration::hours(i64::from(policy.escalate_hours_before))
    };
    if !due {
        return ShowOperationsDecision::Hold(ShowOperationsHoldReason::NotDue);
    }

    let cooldown = Duration::hours(i64::from(policy.escalation_cooldown_hours.max(1)));
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
