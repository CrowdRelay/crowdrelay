//! Causal-model ports: the loaded model plus the provenance of the belief
//! state that produced it. Split from `ports.rs` under the source-size
//! ratchet; re-exported as `ports::causal_model::*`.

use crowdrelay_brain::CausalModel;
use time::OffsetDateTime;

/// A causal model and the identity of the belief state that produced it.
///
/// A decision persists the number the model gave it. Without this it could not
/// say which beliefs produced that number, and the posteriors move every cycle
/// — so re-deriving the estimate later answers "what would the brain predict
/// now", which is a different question that looks identical in a report.
///
/// Two fields rather than a tuple, and the identity is the repository's to
/// report rather than the caller's to derive: the checkpoint row is the
/// repository's to read, and an application layer computing an identity for
/// state it did not load would be guessing at provenance.
#[derive(Clone, Debug)]
pub struct LoadedCausalModel {
    pub model: CausalModel,
    pub belief: BeliefStateOrigin,
}

/// Where the belief state came from, and what identifies it.
///
/// Deliberately two variants. A full replay has no checkpoint, and reporting
/// one would be a fabricated identity — the honest answer is that the model was
/// rebuilt from evidence, and how much of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum BeliefStateOrigin {
    /// Rebuilt from a stored checkpoint, then advanced by the evidence
    /// resolved since it was written.
    ///
    /// `checkpoint_content_hash` is the identity. `brain_state` holds
    /// one row per module, updated in place, with no id and no history — so
    /// `checkpoint_updated_at` says *when* and cannot say *which*, and the
    /// state it labelled is overwritten by the next cycle. The hash is derived
    /// from the content that produced the estimate, so it stays true after the
    /// row moves on: two decisions carrying the same hash used the same
    /// beliefs, and one carrying a different hash did not.
    ///
    /// It identifies, and does not retrieve. Nothing here stores the
    /// checkpoint's bytes, and claiming otherwise would be the lie this type
    /// exists to prevent.
    Checkpoint {
        checkpoint_content_hash: String,
        #[serde(with = "time::serde::rfc3339")]
        checkpoint_updated_at: OffsetDateTime,
        /// Evidence rows applied on top of the checkpoint. The estimate came
        /// from checkpoint plus these, not from the checkpoint alone.
        delta_evidence: u32,
    },
    /// No usable checkpoint existed, so the model was rebuilt from evidence.
    FullReplay { evidence_replayed: u32 },
}
