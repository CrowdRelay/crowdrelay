//! How much measured evidence each context has behind it.
//!
//! Its own module rather than a section of `model.rs`: the authority gate asks
//! two separate questions of a context, and the answer to "how confident are
//! you" already had a home while the answer to "what was that computed from"
//! did not.

use std::collections::BTreeMap;

use crowdrelay_domain::autonomy::EvidenceCount;

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
}

impl EvidenceLedger {
    /// Builds a ledger from `(context, resolved outcome count)` pairs.
    #[must_use]
    pub fn from_counts(counts: BTreeMap<AutopilotContext, i64>) -> Self {
        Self { counts }
    }

    /// The count for one context. Absent means none were ever measured.
    #[must_use]
    pub fn for_context(&self, context: AutopilotContext) -> EvidenceCount {
        EvidenceCount(self.counts.get(&context).copied().unwrap_or(0))
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
