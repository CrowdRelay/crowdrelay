#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(expires_at: OffsetDateTime, items: Vec<(&str, i32)>) -> ReserveInventoryRequest {
        ReserveInventoryRequest {
            external_reference: "checkout-123".to_owned(),
            expires_at,
            items: items
                .into_iter()
                .map(|(sku, quantity)| ReserveInventoryItemRequest {
                    sku: sku.to_owned(),
                    quantity,
                })
                .collect(),
        }
    }

    #[test]
    fn reservation_hash_is_stable_across_expiry_retries() {
        let first = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-B", 1), ("SKU-A", 2)],
        );
        let second = reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(31),
            vec![("SKU-A", 2), ("SKU-B", 1)],
        );
        let first = normalize_reservation(first);
        let second = normalize_reservation(second);
        assert!(first.is_ok());
        assert!(second.is_ok());
        let Ok(first) = first else { return };
        let Ok(second) = second else { return };
        let first_hash = reservation_request_hash(&first);
        let second_hash = reservation_request_hash(&second);
        assert!(first_hash.is_ok());
        assert!(second_hash.is_ok());
        let Ok(first_hash) = first_hash else { return };
        let Ok(second_hash) = second_hash else { return };
        assert_eq!(first_hash, second_hash);
    }

    #[test]
    fn reservation_normalization_merges_duplicate_skus() {
        let normalized = normalize_reservation(reservation(
            OffsetDateTime::now_utc() + TimeDuration::minutes(30),
            vec![("SKU-A", 1), ("SKU-B", 1), ("SKU-A", 2)],
        ));
        assert!(normalized.is_ok());
        let Ok(normalized) = normalized else { return };

        assert_eq!(normalized.items.len(), 2);
        assert_eq!(normalized.items[0].sku, "SKU-A");
        assert_eq!(normalized.items[0].quantity, 3);
        assert_eq!(normalized.items[1].sku, "SKU-B");
    }

    #[test]
    fn product_slug_is_canonicalized_before_persistence() {
        let normalized = normalize_slug("  Echoes-CD  ");
        assert_eq!(normalized.as_deref(), Ok("echoes-cd"));
        assert!(matches!(
            normalize_slug("echoes cd"),
            Err(CommerceError::Invalid)
        ));
    }

    #[test]
    fn public_merch_etag_ignores_generated_at() {
        let first = MerchCatalogView {
            generated_at: OffsetDateTime::UNIX_EPOCH,
            products: Vec::new(),
        };
        let second = MerchCatalogView {
            generated_at: OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
            products: Vec::new(),
        };
        assert_eq!(merch_catalog_etag(&first), merch_catalog_etag(&second));
    }

    #[test]
    fn public_merch_etag_accepts_weak_client_revalidation() {
        let catalog = MerchCatalogView {
            generated_at: OffsetDateTime::UNIX_EPOCH,
            products: Vec::new(),
        };
        let Some(etag) = merch_catalog_etag(&catalog) else {
            assert!(false, "serializable empty merch catalog must have an etag");
            return;
        };
        let weak = HeaderValue::from_str(&format!("W/{etag}"));
        assert!(weak.is_ok());
        let Ok(weak) = weak else { return };
        assert!(merch_etag_matches(Some(&weak), &etag));
    }

    #[test]
    fn unsafe_inventory_mutation_keys_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY,
            axum::http::HeaderValue::from_static("tiny"),
        );
        assert!(matches!(
            idempotency_key(&headers),
            Err(CommerceError::Invalid)
        ));
    }
}
