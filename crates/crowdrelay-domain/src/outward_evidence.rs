//! The proof an outward send carries, or the reason it does not send.
//!
//! An outward action — owned-audience or third-party — reaches a person and
//! cannot be unsent. For a long time the only record of *why* a send was
//! justified lived inside whichever payload arm happened to emit it, and the
//! dispatch funnel that every send passes through checked nothing. A wrong
//! segment or a boilerplate pitch therefore looked identical to a considered
//! one — the machine could not tell them apart because nothing asked it to.
//!
//! `OutwardEvidence` is the answer, as a type rather than a convention: an
//! outward send that cannot say where it came from, why this recipient, and
//! when that recipient was last contacted is refused at dispatch. The third
//! field is the answer to a question, not a fact that must exist — a first
//! contact has no `last_contact_at`, and `None` is the honest value for it.
//! What the type makes impossible is sending *without having asked*.

use serde::{Deserialize, Serialize};

/// Why a send has no evidence, or why the evidence it has is not enough.
///
/// Each variant is a sentence the execution journal can record verbatim — the
/// refusal is the finding, and "a send vanished" is the failure this exists to
/// prevent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceRefusal {
    /// The dispatch reached the emit path with no evidence attached.
    Missing,
    /// `source_id` empty — the send cannot name what justified it.
    NoSource,
    /// `recipient_reason` empty — the send cannot say why this recipient.
    NoRecipientReason,
}

impl EvidenceRefusal {
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::Missing => "outward send refused: no evidence attached",
            Self::NoSource => "outward send refused: evidence names no source",
            Self::NoRecipientReason => {
                "outward send refused: evidence does not say why this recipient"
            }
        }
    }
}

/// The three facts an outward send must be able to state.
///
/// Construct through [`OutwardEvidence::new`] — the fields stay private so an
/// empty `source_id` or `recipient_reason` cannot be assembled by accident.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutwardEvidence {
    /// What produced this send: the decision, source, target or campaign row
    /// the payload can be traced back to.
    source_id: String,
    /// Why *this* recipient — "verified pitch route, accepts outreach on a
    /// published-form basis", not "they seemed right".
    recipient_reason: String,
    /// When any outward action last touched this recipient's subject.
    /// `None` means *never contacted* — a real answer, not a missing one.
    /// Filled at dispatch from the durable action rows: the call site cannot
    /// know it without a query, and a claimed value would be evidence the
    /// system asserted rather than measured.
    last_contact_at: Option<time::OffsetDateTime>,
}

impl OutwardEvidence {
    /// Builds evidence from the two facts only the call site knows.
    /// `last_contact_at` starts `None`; [`with_last_contact`] fills it at
    /// dispatch from the subject's touch history.
    ///
    /// # Errors
    ///
    /// Refuses a blank `source_id` or `recipient_reason` — evidence that
    /// cannot name its source or its reason is no evidence.
    pub fn new(
        source_id: impl Into<String>,
        recipient_reason: impl Into<String>,
    ) -> Result<Self, EvidenceRefusal> {
        let source_id = source_id.into();
        let recipient_reason = recipient_reason.into();
        if source_id.trim().is_empty() {
            return Err(EvidenceRefusal::NoSource);
        }
        if recipient_reason.trim().is_empty() {
            return Err(EvidenceRefusal::NoRecipientReason);
        }
        Ok(Self {
            source_id,
            recipient_reason,
            last_contact_at: None,
        })
    }

    /// Re-attaches the third fact once dispatch has measured it.
    #[must_use]
    pub const fn with_last_contact(mut self, at: Option<time::OffsetDateTime>) -> Self {
        self.last_contact_at = at;
        self
    }

    #[must_use]
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    #[must_use]
    pub fn recipient_reason(&self) -> &str {
        &self.recipient_reason
    }

    #[must_use]
    pub const fn last_contact_at(&self) -> Option<time::OffsetDateTime> {
        self.last_contact_at
    }

    /// Reads evidence back out of a serialized payload — the gate's half of
    /// the contract. A payload whose `evidence` fails to deserialize, or
    /// deserializes to empty facts, refuses exactly as a missing one does.
    ///
    /// # Errors
    ///
    /// `Missing` when the key is absent; the typed refusals when the facts
    /// are blank.
    pub fn from_payload(payload: &serde_json::Value) -> Result<Self, EvidenceRefusal> {
        // `send_evidence`, not `evidence`: `RequestOutreach` payloads already
        // carry `evidence` — the band-numbers packet the pitch quotes — and
        // reading that here would refuse every wave send as "missing".
        let Some(value) = payload.get("send_evidence") else {
            return Err(EvidenceRefusal::Missing);
        };
        let Ok(evidence) = serde_json::from_value::<Self>(value.clone()) else {
            return Err(EvidenceRefusal::Missing);
        };
        Self::new(evidence.source_id, evidence.recipient_reason)
            .map(|built| built.with_last_contact(evidence.last_contact_at))
    }

    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_with_all_three_facts_serializes_and_reads_back() {
        let at = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts");
        let evidence = OutwardEvidence::new("target:abc", "verified pitch route")
            .expect("valid evidence")
            .with_last_contact(Some(at));
        let payload = serde_json::json!({"draft": {}, "send_evidence": evidence.to_json()});
        let read = OutwardEvidence::from_payload(&payload).expect("round trips");
        assert_eq!(read.source_id(), "target:abc");
        assert_eq!(read.last_contact_at(), Some(at));
    }

    #[test]
    fn a_first_contact_carries_never_as_an_answer_not_an_absence() {
        let evidence = OutwardEvidence::new("target:abc", "basis: published form")
            .expect("valid")
            .with_last_contact(None);
        let payload = serde_json::json!({"send_evidence": evidence.to_json()});
        let read = OutwardEvidence::from_payload(&payload).expect("round trips");
        assert_eq!(read.last_contact_at(), None);
    }

    #[test]
    fn missing_evidence_refuses() {
        let payload = serde_json::json!({"draft": {"body": "hi"}});
        assert_eq!(
            OutwardEvidence::from_payload(&payload),
            Err(EvidenceRefusal::Missing)
        );
    }

    #[test]
    fn blank_facts_refuse_at_construction_and_at_read() {
        assert_eq!(
            OutwardEvidence::new("  ", "reason"),
            Err(EvidenceRefusal::NoSource)
        );
        let payload = serde_json::json!({
            "send_evidence": {"source_id": "s", "recipient_reason": " ", "last_contact_at": null}
        });
        assert_eq!(
            OutwardEvidence::from_payload(&payload),
            Err(EvidenceRefusal::NoRecipientReason)
        );
    }
}
