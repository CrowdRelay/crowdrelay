//! Treatment assignment for evidence recording.
//!
//! The brain records whether each dispatch was treatment (dispatched) or
//! control (withheld) as part of the evidence row. This enables off-policy
//! evaluation of dispatch decisions.

use serde::{Deserialize, Serialize};

/// The treatment assignment for a dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreatmentAssignment {
    /// The dispatch was performed (treatment group).
    Treatment,
    /// The dispatch was withheld (control group).
    Control,
}

impl TreatmentAssignment {
    /// Returns 1.0 for treatment, 0.0 for control — used in IPW calculations.
    #[must_use]
    pub const fn indicator(self) -> f64 {
        match self {
            Self::Treatment => 1.0,
            Self::Control => 0.0,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Treatment => "treatment",
            Self::Control => "control",
        }
    }
}
