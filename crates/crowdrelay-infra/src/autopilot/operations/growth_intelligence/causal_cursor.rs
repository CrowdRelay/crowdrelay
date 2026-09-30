//! A checkpoint may only consume measurements actually included in its state.

use crowdrelay_brain::CausalModel;
use time::OffsetDateTime;

pub(super) fn advance(model: &mut CausalModel, read_cursor: Option<OffsetDateTime>) {
    model.evidence_cursor = model.evidence_cursor.into_iter().chain(read_cursor).max();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_time_cannot_advance_the_learning_cursor() {
        let mut model = CausalModel::default();
        let original = model.evidence_cursor;
        advance(&mut model, None);
        let state = serde_json::to_value(&model).unwrap();
        assert!(state["evidence_cursor"].is_string());
        let restored: CausalModel = serde_json::from_value(state.clone()).unwrap();
        assert_eq!(restored.evidence_cursor, original);
        let mut tuple_state = state.clone();
        tuple_state["evidence_cursor"] = serde_json::to_value(original).unwrap();
        let tuple_restored: CausalModel = serde_json::from_value(tuple_state).unwrap();
        assert_eq!(tuple_restored.evidence_cursor, original);
        let mut legacy = state;
        legacy.as_object_mut().unwrap().remove("evidence_cursor");
        let legacy: CausalModel = serde_json::from_value(legacy).unwrap();
        assert!(legacy.evidence_cursor.is_none());
    }
}
