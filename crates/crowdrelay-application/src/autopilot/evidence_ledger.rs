//! How much measured evidence each context has behind it.
//!
//! Its own module rather than a section of `model.rs`: the authority gate asks
//! two separate questions of a context, and the answer to "how confident are
//! you" already had a home while the answer to "what was that computed from"
//! did not.

use std::collections::BTreeMap;

use crowdrelay_domain::autonomy::{BootstrapAllowance, ContextEvidence, EvidenceCount};

use super::model::AutopilotContext;

/// How much measured evidence each context has behind it, read once per cycle.
///
/// The authority gate asks a context how confident it is and, until this
/// existed, had no way to ask what that confidence was computed from. The two
/// come apart exactly where it costs most: a context that has dispatched four
/// times and had four outcomes measured can report a high confidence, clear
/// its minimum, and be handed unattended execution over an action the tenant
/// cannot take back.
///
/// A context absent from the ledger has [`EvidenceCount::NONE`], which is the
/// honest reading — never measured is not the same as measured at zero, but
/// for the purpose of "may this run unattended" they are the same answer, and
/// this type deliberately does not invent a number for either.
#[derive(Clone, Debug, Default)]
pub struct EvidenceLedger {
    counts: BTreeMap<AutopilotContext, i64>,
    /// Unattended actions each context has already taken in the trailing
    /// week, counted from the durable action rows. Absent means none.
    bootstrap_spent: BTreeMap<AutopilotContext, i64>,
    /// The operator's weekly warm-up cap, the same for every context. Zero
    /// until an envelope is attached, so a ledger built without one grants no
    /// warm-up rather than an unbounded one.
    bootstrap_cap: i64,
}

impl EvidenceLedger {
    /// Builds a ledger from `(context, resolved outcome count)` pairs.
    ///
    /// The warm-up is absent until [`Self::with_bootstrap`] attaches it, which
    /// means a caller that forgets it gets no warm-up at all. That is the
    /// right direction to fail in: the floor without a warm-up is the
    /// behaviour this ledger already had.
    #[must_use]
    pub fn from_counts(counts: BTreeMap<AutopilotContext, i64>) -> Self {
        Self {
            counts,
            bootstrap_spent: BTreeMap::new(),
            bootstrap_cap: 0,
        }
    }

    /// Attaches the operator's weekly warm-up cap and what each context has
    /// already spent against it.
    #[must_use]
    pub fn with_bootstrap(mut self, spent: BTreeMap<AutopilotContext, i64>, cap: u32) -> Self {
        self.bootstrap_spent = spent;
        self.bootstrap_cap = i64::from(cap);
        self
    }

    /// What one context has learned, and what it may still spend learning.
    ///
    /// Absent from either map means zero: never measured and nothing spent.
    #[must_use]
    pub fn for_context(&self, context: AutopilotContext) -> ContextEvidence {
        ContextEvidence {
            observations: EvidenceCount(self.counts.get(&context).copied().unwrap_or(0)),
            bootstrap: BootstrapAllowance {
                spent: self.bootstrap_spent.get(&context).copied().unwrap_or(0),
                cap: self.bootstrap_cap,
            },
        }
    }

    /// Every context that has at least one measured outcome, for the posture
    /// read. Contexts with none are deliberately omitted rather than rendered
    /// as zero rows.
    pub fn measured(&self) -> impl Iterator<Item = (AutopilotContext, i64)> + '_ {
        self.counts
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(context, count)| (*context, *count))
    }
}
