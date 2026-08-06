#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_drop_ids() {
        assert!(valid_drop_id("wro-001"));
        assert!(valid_drop_id("tor-012"));
        assert!(!valid_drop_id("WRO-001"));
        assert!(!valid_drop_id("wro-01"));
    }

    #[test]
    fn idempotency_key_must_be_a_uuid() -> Result<(), axum::http::header::InvalidHeaderValue> {
        let mut headers = HeaderMap::new();
        assert!(!valid_idempotency_key(&headers));
        headers.insert(IDEMPOTENCY_KEY.clone(), "not-a-uuid".parse()?);
        assert!(!valid_idempotency_key(&headers));
        headers.insert(IDEMPOTENCY_KEY.clone(), Uuid::now_v7().to_string().parse()?);
        assert!(valid_idempotency_key(&headers));
        Ok(())
    }

    #[test]
    fn distance_is_zero_for_same_position() {
        assert!(distance_meters(51.0, 17.0, 51.0, 17.0) < 0.01);
    }

    #[test]
    fn median_handles_even_and_odd_samples() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(Vec::new()), None);
    }
}
