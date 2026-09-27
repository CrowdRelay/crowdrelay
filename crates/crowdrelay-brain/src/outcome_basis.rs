//! What a growth-evidence row's fan columns count. Split from `evidence`
//! to keep that file inside the source-size ratchet.

use serde::{Deserialize, Serialize};

/// What a row's fan columns (`observed_fans`, the incremental and durable
/// estimates) count.
///
/// Rows written before 2026-09-27 counted every fan the workspace gained in
/// the action's window, so each dispatch was credited with arrivals from any
/// cause and overlapping dispatches with the same ones — 145 fan-observations
/// against 23 fans ever. Those rows are `WorkspaceWindow`, and nothing that
/// learns *what one dispatch did* may read them: the outcome model, the
/// Y14/Y30 treatment effects, the bridge, the family effects, the control-arm
/// contrast and the strategy posterior all skip them. Their secondary
/// metrics (clicks, engagement, revenue) were always per-action and still
/// teach. New rows are `Attributed`: fans traced to the action's own links.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeBasis {
    #[default]
    Attributed,
    WorkspaceWindow,
}

impl OutcomeBasis {
    /// Reads the column. Anything unrecognised is `WorkspaceWindow`: a row
    /// whose basis cannot be established does not teach per-action learners.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "attributed" => Self::Attributed,
            _ => Self::WorkspaceWindow,
        }
    }

    /// Whether the row's fan columns describe what this one dispatch did.
    #[must_use]
    pub const fn teaches_per_action(self) -> bool {
        matches!(self, Self::Attributed)
    }
}
