//! A checkpoint may only consume measurements actually included in its state.

use crowdrelay_brain::{CausalModel, GrowthEvidence};

pub(super) fn advance(model: &mut CausalModel, evidence: &[GrowthEvidence]) {
    model.evidence_cursor = model
        .evidence_cursor
        .into_iter()
        .chain(evidence.iter().flat_map(|ev| {
            [
                ev.resolved_at,
                ev.replayed_3d_at,
                ev.replayed_14d_at,
                ev.replayed_30d_at,
            ]
            .into_iter()
            .flatten()
        }))
        .max();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_time_cannot_advance_the_learning_cursor() {
        let mut model = CausalModel::default();
        let original = model.evidence_cursor;
        advance(&mut model, &[]);
        let state = serde_json::to_value(&model).unwrap();
        let restored: CausalModel = serde_json::from_value(state.clone()).unwrap();
        assert_eq!(restored.evidence_cursor, original);
        let mut legacy = state;
        legacy.as_object_mut().unwrap().remove("evidence_cursor");
        let legacy: CausalModel = serde_json::from_value(legacy).unwrap();
        assert!(legacy.evidence_cursor.is_none());
    }
}
