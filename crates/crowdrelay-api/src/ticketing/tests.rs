#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_snapshot_separates_sales_holds_and_availability() -> Result<(), TicketingError> {
        assert_eq!(
            inventory_snapshot(100, 12, 3)?,
            InventorySnapshot {
                sold: 12,
                reserved: 3,
                available: 85,
            }
        );
        Ok(())
    }

    #[test]
    fn inventory_snapshot_rejects_negative_or_overcommitted_state() {
        assert!(inventory_snapshot(-1, 0, 0).is_err());
        assert!(inventory_snapshot(100, -1, 0).is_err());
        assert!(inventory_snapshot(100, 0, -1).is_err());
        assert!(inventory_snapshot(10, 8, 3).is_err());
    }

    #[test]
    fn type_inventory_checks_commitment_overflow() {
        assert_eq!(
            TypeInventory {
                reserved: 2,
                sold: 7,
            }
            .committed(),
            Ok(9)
        );
        assert_eq!(
            TypeInventory {
                reserved: i64::MAX,
                sold: 1,
            }
            .committed(),
            Err(TicketingError::Unexpected)
        );
    }

    #[test]
    fn email_masking_never_exposes_the_local_part() {
        assert_eq!(mask_email("wojciech@gmail.com"), "w***@gmail.com");
        assert_eq!(mask_email("a@example.org"), "a***@example.org");
        assert_eq!(mask_email("invalid"), "***");
    }

    #[test]
    fn splits_vat_inclusive_price_with_half_up_rounding() {
        assert_eq!(split_gross(5_000, 800), Ok((4_630, 370)));
        assert_eq!(split_gross(1_000, 800), Ok((926, 74)));
        assert_eq!(split_gross(0, 800), Ok((0, 0)));
    }

    #[test]
    fn checkout_token_is_deterministic_and_context_bound() -> Result<(), TicketingError> {
        let key = [7_u8; 32];
        let order = Uuid::now_v7();
        let first = derive_checkout_token(&key, order, "reservation-1")?;
        assert_eq!(first, derive_checkout_token(&key, order, "reservation-1")?);
        assert_ne!(first, derive_checkout_token(&key, order, "reservation-2")?);
        assert_eq!(first.len(), 64);
        Ok(())
    }

    #[test]
    fn checkout_token_validation_is_strict() {
        assert!(valid_checkout_token(&"a".repeat(64)));
        assert!(valid_checkout_token(&"F".repeat(64)));
        assert!(!valid_checkout_token(&"a".repeat(63)));
        assert!(!valid_checkout_token(&"a".repeat(65)));
        assert!(!valid_checkout_token(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn order_reference_does_not_expose_full_uuid() {
        let order = Uuid::now_v7();
        let reference = order_public_reference(order);
        assert!(reference.starts_with("VRY-ORD-"));
        assert_eq!(reference.len(), 24);
        assert_eq!(reference.matches('-').count(), 2);
    }

    #[test]
    fn stripe_identifiers_are_strictly_bounded() {
        assert!(valid_stripe_id("cs_test_123ABC", "cs_"));
        assert!(!valid_stripe_id("pi_123", "cs_"));
        assert!(!valid_stripe_id("cs_bad/value", "cs_"));
    }
}
