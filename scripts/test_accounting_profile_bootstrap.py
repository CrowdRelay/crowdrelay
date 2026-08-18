from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


class AccountingProfileBootstrapContract(unittest.TestCase):
    def test_preview_is_available_before_profile_configuration(self):
        core = (ROOT / "crates/crowdrelay-api/src/accounting/core.rs").read_text()
        preview = core[
            core.index("async fn build_preview("):
            core.index("async fn load_sales(")
        ]
        self.assertIn("load_profile_optional(state)", preview)
        self.assertIn(".unwrap_or_else(unconfigured_profile)", preview)
        self.assertNotIn("let profile = load_profile(state).await?;", preview)

    def test_placeholder_is_safe_and_not_persisted(self):
        core = (ROOT / "crates/crowdrelay-api/src/accounting/core.rs").read_text()
        placeholder = core[
            core.index("fn unconfigured_profile()"):
            core.index("async fn load_profile_optional(")
        ]
        self.assertIn("seller_name: String::new()", placeholder)
        self.assertIn("tax_id: String::new()", placeholder)
        self.assertIn("country_code: default_country_code()", placeholder)
        self.assertIn("document_prefix: default_document_prefix()", placeholder)
        self.assertIn("updated_at: OffsetDateTime::UNIX_EPOCH", placeholder)
        self.assertNotIn("INSERT", placeholder)
        self.assertNotIn("UPDATE", placeholder)

    def test_finalization_remains_fail_closed_without_real_profile(self):
        core = (ROOT / "crates/crowdrelay-api/src/accounting/core.rs").read_text()
        tx_preview = core[
            core.index("async fn build_preview_tx("):
            core.index("async fn load_sales_tx(")
        ]
        self.assertIn("FROM ticket_accounting_profiles", tx_preview)
        self.assertIn(".ok_or(AccountingError::NotFound)?", tx_preview)


if __name__ == "__main__":
    unittest.main()
