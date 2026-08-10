from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
DOMAIN = ROOT / "crates/crowdrelay-domain/src"
APP_ROOT = ROOT / "crates/crowdrelay-application/src"
INFRA_ROOT = ROOT / "crates/crowdrelay-infra/src"
MIGRATIONS = (
    ROOT / "migrations/0033_viryaos_autopilot.sql",
    ROOT / "migrations/0034_viryaos_operations.sql",
    ROOT / "migrations/0035_viryaos_queue_index_alignment.sql",
)

def module_tree(root: Path, stem: str) -> str:
    parts = [(root / f"{stem}.rs").read_text()]
    directory = root / stem
    if directory.exists():
        parts.extend(path.read_text() for path in sorted(directory.rglob("*.rs")))
    return "\n".join(parts)

APP_TEXT = module_tree(APP_ROOT, "autopilot")
INFRA_TEXT = module_tree(INFRA_ROOT, "autopilot")
MIGRATION_TEXT = "\n".join(path.read_text() for path in MIGRATIONS)
WORKER = ROOT / "crates/crowdrelay-worker/src/autopilot.rs"
WORKER_BOOTSTRAP = ROOT / "crates/crowdrelay-worker/src/bootstrap/persistence.rs"
COMPOSE = ROOT / "docker-compose.yml"
CI = ROOT / ".github/workflows/ci.yml"
OUTBOX = ROOT / "crates/crowdrelay-worker/src/outbox/repository.rs"
OPENAPI = ROOT / "openapi/openapi.yaml"
OPS = ROOT / "crates/crowdrelay-api/src/ops.rs"

class ViryaOsAutopilotV1(unittest.TestCase):
    def test_domain_boundaries_are_pure_and_explicit(self):
        for name in ("autonomy", "pricing", "merchandising", "audience_lifecycle", "booking", "promotion", "market_intelligence", "performance", "campaign_lifecycle", "outreach", "content_supply", "experimentation", "show_operations", "merch_bundle"):
            text = (DOMAIN / f"{name}.rs").read_text()
            self.assertNotIn("sqlx::", text, name)
            self.assertNotIn("reqwest::", text, name)
            self.assertNotIn("serde_json::Value", text, name)
            self.assertNotIn("tokio::", text, name)
            self.assertRegex(text, r"#\[cfg\(test\)\]")

    def test_application_has_separate_decision_action_and_control_ports(self):
        text = APP_TEXT
        self.assertIn("trait AutopilotDecisionRepository", text)
        self.assertIn("trait AutopilotActionRepository", text)
        self.assertIn("trait AutopilotControlRepository", text)
        self.assertIn("trait AutopilotMarketStateRepository", text)
        self.assertIn("trait AutopilotMerchStateRepository", text)
        self.assertNotIn("trait AutopilotRepository", text)
        self.assertIn("decision_key", text)
        self.assertIn("action_idempotency_key", text)
        self.assertIn("policy_evidence", text)
        self.assertIn("minimum_confidence_basis_points", text)

    def test_decision_and_side_effect_idempotency_are_distinct(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        self.assertIn("UNIQUE (workspace_id, decision_key)", migration)
        self.assertIn("UNIQUE (workspace_id, idempotency_key)", migration)
        self.assertIn("viryaos_autopilot_action_emissions", migration)
        self.assertIn("ON CONFLICT (workspace_id, decision_key) DO NOTHING", infra)
        self.assertIn("ON CONFLICT (workspace_id, emission_key) DO NOTHING", infra)

    def test_autonomy_is_fail_closed_and_operator_versioned(self):
        migration = MIGRATION_TEXT
        app = APP_TEXT
        infra = INFRA_TEXT
        self.assertIn("enabled boolean NOT NULL DEFAULT false", migration)
        self.assertIn("autonomy_level text NOT NULL DEFAULT 'observe'", migration)
        self.assertIn("expected_version", app)
        self.assertRegex(infra, r"WHERE workspace_id = \$1 AND context = \$2 AND version = \$7")

    def test_worker_recovers_stale_actions_and_bounds_retries(self):
        infra = INFRA_TEXT
        self.assertIn("FOR UPDATE SKIP LOCKED", infra)
        self.assertIn("started_at <= $2 - INTERVAL '15 minutes'", infra)
        self.assertIn("attempt_count < 5", infra)
        self.assertIn("stale_retry_exhausted", infra)
        self.assertIn("INTERVAL '5 minutes'", infra)
        self.assertIn("record_execution_outcome", infra)

    def test_lifecycle_respects_existing_marketing_and_delivery_time_consent(self):
        infra = INFRA_TEXT
        outbox = OUTBOX.read_text()
        self.assertIn("communication_campaign_recipients", infra)
        self.assertIn("campaign.status IN ('scheduled', 'completed')", infra)
        self.assertNotIn("synesthesia.completed_at <= $2 - INTERVAL '48 hours'", infra)
        self.assertIn('"viryaos.fan_lifecycle.message_requested"', outbox)
        self.assertIn("delivery_is_eligible", outbox)

    def test_postgres_18_aio_and_volume_layout_are_pinned(self):
        compose = COMPOSE.read_text()
        ci = CI.read_text()
        self.assertIn("postgres:18-alpine", compose)
        self.assertIn('io_method=worker', compose)
        self.assertIn('CROWDRELAY_POSTGRES_IO_WORKERS:-3', compose)
        self.assertIn('CROWDRELAY_POSTGRES_EFFECTIVE_IO_CONCURRENCY:-16', compose)
        self.assertIn('CROWDRELAY_POSTGRES_MAINTENANCE_IO_CONCURRENCY:-16', compose)
        self.assertIn("crowdrelay_postgres18:/var/lib/postgresql", compose)
        self.assertNotIn("crowdrelay_postgres:/var/lib/postgresql", compose)
        self.assertNotIn("/var/lib/postgresql/data", compose)
        self.assertIn("postgres:18-alpine", ci)


    def test_queue_indexes_match_workspace_scoped_hot_paths(self):
        migration = (ROOT / "migrations/0035_viryaos_queue_index_alignment.sql").read_text()
        self.assertIn("(workspace_id, available_at, id)", migration)
        self.assertIn("WHERE status = 'queued' AND attempt_count < 5", migration)
        self.assertIn("(workspace_id, due_at, available_at, id)", migration)
        self.assertIn("WHERE status = 'pending' AND attempt_count < 3", migration)
        self.assertIn("viryaos_autopilot_actions_processing_idx", migration)
        self.assertIn("viryaos_autopilot_measurements_processing_idx", migration)

    def test_control_plane_and_pg18_runtime_are_public_contracts(self):
        openapi = OPENAPI.read_text()
        ops = OPS.read_text()
        for path in (
            "/admin/autopilot/overview",
            "/admin/autopilot/policies/{context}",
            "/admin/autopilot/actions/{action_id}/approve",
            "/admin/autopilot/actions/{action_id}/cancel",
            "/admin/autopilot/booking-targets",
            "/admin/autopilot/merch-economics",
            "/admin/autopilot/promotion-state",
            "/admin/autopilot/market-signals/city",
        ):
            self.assertIn(path, openapi)
        self.assertIn("runtime_enabled", openapi)
        self.assertIn("DatabaseRuntimeSummary", openapi)
        self.assertIn("current_setting('io_method', true)", ops)
        self.assertIn("current_setting('io_combine_limit', true)", ops)
        self.assertIn("current_setting('io_max_combine_limit', true)", ops)
        self.assertNotIn("FROM pg_aios", ops)
        self.assertNotIn("aio_inflight", openapi)
        self.assertIn("async_io_active", ops)
        self.assertIn("promotion_budget", openapi)
        self.assertIn("request_promotion_budget_change", openapi)


    def test_promotion_state_is_fresh_audited_and_history_preserving(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        app = APP_TEXT
        self.assertIn("viryaos_promotion_campaign_states", migration)
        self.assertIn("viryaos_promotion_campaign_observations", migration)
        self.assertIn("AutopilotMarketStateRepository", app)
        self.assertIn("upsert_autopilot_promotion_state", infra)
        self.assertIn("EXCLUDED.observed_at > viryaos_promotion_campaign_states.observed_at", infra)
        self.assertIn("viryaos.promotion.budget_change_requested", infra)
        self.assertIn("ensure_promotion_state_current", infra)


    def test_market_signals_are_typed_fresh_and_only_bounded_booking_evidence(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        booking = (DOMAIN / "booking.rs").read_text()
        market = (DOMAIN / "market_intelligence.rs").read_text()
        self.assertIn("viryaos_city_market_signals", migration)
        self.assertIn("viryaos_city_market_signal_observations", migration)
        self.assertIn("EXCLUDED.observed_at > viryaos_city_market_signals.observed_at", infra)
        self.assertIn("aggregate_city_market_evidence", infra)
        self.assertIn("signal_families", market)
        self.assertIn(".min(10)", booking)
        self.assertIn("external_market_evidence_is_only_a_bounded_confirmation_bonus", booking)

    def test_hot_snapshot_indexes_match_autopilot_queries(self):
        migration = MIGRATION_TEXT
        self.assertIn("communication_campaign_recipients_autopilot_fan_idx", migration)
        self.assertIn("viryaos_autopilot_actions_subject_history_idx", migration)
        self.assertIn("WHERE status = 'paid'", migration)


    def test_booking_outreach_uses_verified_versioned_targets_not_n8n_recipient_choice(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        app = APP_TEXT
        booking = (DOMAIN / "booking.rs").read_text()
        openapi = OPENAPI.read_text()
        self.assertIn("viryaos_booking_targets", migration)
        self.assertIn("viryaos_booking_target_history", migration)
        self.assertIn("BookingTargetSelectionPolicy", booking)
        self.assertIn("target_cooldown_days: 180", booking)
        self.assertIn("select_booking_target", booking)
        self.assertIn("AutopilotBookingStateRepository", app)
        self.assertIn("target_version", app)
        self.assertIn("load_booking_target_snapshots", infra)
        self.assertIn("lock_booking_target_for_execution", infra)
        self.assertIn('BookingOutreachPhase::Initial => "booking.opportunity.v1"', infra)
        self.assertIn('BookingOutreachPhase::FollowUp => "booking.followup.v1"', infra)
        self.assertIn('"contact_email": target.2', infra)
        self.assertIn("upsert_autopilot_booking_target", infra)
        self.assertIn("/admin/autopilot/booking-targets", openapi)

    def test_merch_yield_is_bounded_versioned_and_margin_guarded(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        app = APP_TEXT
        domain = (DOMAIN / "merchandising.rs").read_text()
        openapi = OPENAPI.read_text()
        self.assertIn("MerchPricePolicy", domain)
        self.assertIn("minimum_gross_margin_basis_points", domain)
        self.assertIn("economics_version", domain)
        self.assertIn("recent_price_change_suppresses_churn", domain)
        self.assertIn("margin_floor_prevents_discount_below_safe_economics", domain)
        self.assertIn("viryaos_merch_product_economics", migration)
        self.assertIn("viryaos_merch_product_economics_history", migration)
        self.assertIn("merch_pricing", migration)
        self.assertIn("ChangeMerchPrice", app)
        self.assertIn("economics_version: snapshot.economics_version", app)
        self.assertIn("execute_merch_price_change", infra)
        self.assertIn("guardrails.2 != expected_economics_version", infra)
        self.assertIn("price_gross_minor = $3", infra)
        self.assertIn("/admin/autopilot/merch-economics", openapi)
        self.assertIn("change_merch_price", openapi)

    def test_action_quota_is_atomic_visible_and_preserves_future_reevaluation(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        app = APP_TEXT
        openapi = OPENAPI.read_text()
        self.assertIn("max_actions_24h", migration)
        self.assertIn("viryaos_provision_autopilot_policies", migration)
        self.assertIn("AFTER INSERT ON workspaces", migration)
        self.assertIn("SELECT max_actions_24h", infra)
        self.assertIn("FOR UPDATE", infra)
        self.assertIn("actions_24h >=", infra)
        self.assertIn("quota_throttled: true", infra)
        self.assertIn("actions_throttled", app)
        self.assertIn('"max_actions_24h": policy.max_actions_24h', app)
        self.assertIn("max_actions_24h", openapi)


    def test_delayed_effect_measurements_are_durable_bounded_and_not_fake_attribution(self):
        migration = MIGRATION_TEXT
        app = APP_TEXT
        infra = INFRA_TEXT
        worker = WORKER.read_text()
        performance = (DOMAIN / "performance.rs").read_text()
        self.assertNotIn("isfinite(", migration)
        self.assertIn("'Infinity'::double precision", migration)
        self.assertIn("viryaos_autopilot_measurements", migration)
        self.assertIn("viryaos_autopilot_measurements_due_idx", migration)
        self.assertIn("measurement_id uuid", migration)
        self.assertIn("effect_assessment text", migration)
        self.assertIn("delta_basis_points integer", migration)
        self.assertIn("merch_gross_proxy_7d", migration)
        self.assertIn("promotion_roas_7d", migration)
        self.assertIn("trait AutopilotMeasurementRepository", app)
        self.assertIn("assess_measurement_effect", app)
        self.assertIn("assess_effect", performance)
        self.assertIn("schedule_effect_measurement", infra)
        self.assertIn("gross-list-price proxy", infra)
        self.assertIn("observed_at >= $3 + INTERVAL '7 days'", infra)
        self.assertIn("FOR UPDATE SKIP LOCKED", infra)
        self.assertIn("stale_processing_recovered", infra)
        self.assertIn("claim_due_measurements", worker)
        self.assertIn("complete_measurement", worker)
        self.assertIn("fail_measurement", worker)
        self.assertIn("recent_effects", app)
        self.assertIn("RecentAutopilotEffect", app)
        self.assertIn("recent_effects", OPENAPI.read_text())
        # Do not manufacture learning for actions without trustworthy attribution.
        scheduling = infra[infra.index("async fn schedule_effect_measurement"):infra.index("async fn record_execution_outcome")]
        self.assertIn("RequestFanLifecycleMessage { .. }", scheduling)
        self.assertIn("RequestMerchReorder { .. }", scheduling)
        self.assertIn("RequestBookingOutreach { .. }", scheduling)
        self.assertIn("RequestContentArtifact { .. }", scheduling)

    def test_original_viryaos_operating_plan_is_feature_complete(self):
        modules = {path.stem for path in DOMAIN.glob("*.rs")}
        for module in (
            "pricing", "merchandising", "audience_lifecycle", "booking",
            "outreach", "content_supply", "promotion", "experimentation",
            "market_intelligence", "performance", "campaign_lifecycle",
            "show_operations", "merch_bundle", "autonomy",
        ):
            self.assertIn(module, modules)

        app = APP_TEXT
        infra = INFRA_TEXT
        expected_actions = (
            "ChangeTicketPrice", "ChangeTicketCapacity",
            "RequestFanLifecycleMessage", "RequestMerchReorder", "ChangeMerchPrice",
            "RequestBookingOutreach", "RequestAudienceCampaign", "RequestMerchBundle",
            "RequestOutreach", "RequestContentArtifact", "AdjustExperiment",
            "CompleteShowTask", "EscalateShowTask", "RequestPromotionBudgetChange",
        )
        for action in expected_actions:
            self.assertIn(action, app)
            self.assertIn(f"AutopilotActionPayload::{action}", infra)

        for context in (
            "ticket_yield", "fan_lifecycle", "campaign_lifecycle",
            "merchandising", "merch_pricing", "merch_bundle",
            "booking_opportunity", "outreach", "content_supply",
            "promotion_budget", "experimentation", "show_operations",
        ):
            self.assertIn(context, MIGRATION_TEXT)

        self.assertIn("load_chief_of_staff", infra)
        self.assertIn("recent_effects", app)
        self.assertIn("market evidence", (DOMAIN / "booking.rs").read_text().lower())

    def test_promotion_financial_authority_is_explicit_and_race_safe(self):
        migration = MIGRATION_TEXT
        infra = INFRA_TEXT
        app = APP_TEXT
        domain = (DOMAIN / "promotion.rs").read_text()
        openapi = OPENAPI.read_text()
        self.assertIn("MissingWorkspaceGuardrail", domain)
        self.assertIn("WorkspaceBudgetCap", domain)
        self.assertIn("viryaos_promotion_budget_guardrails", migration)
        self.assertIn("viryaos_promotion_budget_reservations", migration)
        self.assertIn("maximum_total_daily_budget_minor", migration)
        self.assertIn("maximum_monthly_spend_minor", migration)
        self.assertIn("UpsertPromotionBudgetGuardrail", app)
        self.assertIn("upsert_promotion_budget_guardrail", infra)
        self.assertIn("FOR UPDATE", infra)
        self.assertIn("reserved_delta_minor", infra)
        self.assertIn("projected_daily", infra)
        self.assertIn("spend_month_to_date_minor", infra)
        self.assertIn("/admin/autopilot/promotion-budget-guardrails", openapi)

    def test_content_supply_includes_provider_neutral_live_listing(self):
        domain = (DOMAIN / "content_supply.rs").read_text()
        infra = INFRA_TEXT
        self.assertIn("LiveListing", domain)
        self.assertIn('"content.live_listing.v1"', domain)
        self.assertIn('"live_listing"', infra)

    def test_autopilot_does_not_expand_the_rust_compile_graph(self):
        domain_manifest = (ROOT / "crates/crowdrelay-domain/Cargo.toml").read_text()
        application_manifest = (ROOT / "crates/crowdrelay-application/Cargo.toml").read_text()
        workspace_manifest = (ROOT / "Cargo.toml").read_text()
        for heavy in ("sqlx", "tokio", "reqwest", "axum", "rand", "candle", "ort"):
            self.assertNotRegex(domain_manifest, rf"(?m)^\s*{heavy}\s*=")
        for heavy in ("sqlx", "reqwest", "axum", "rand", "candle", "ort"):
            self.assertNotRegex(application_manifest, rf"(?m)^\s*{heavy}\s*=")
        self.assertNotIn("crowdrelay-autopilot", workspace_manifest)
        self.assertIn("mod control;", (APP_ROOT / "autopilot.rs").read_text())
        self.assertFalse(any((APP_ROOT / "autopilot").glob("mod.rs")))

    def test_domain_ids_no_longer_have_runtime_type_wrapper_test(self):
        ids = (DOMAIN / "ids.rs").read_text()
        self.assertNotIn("typed_ids_are_not_interchangeable", ids)

    def test_all_bounded_contexts_are_provisioned_for_new_workspaces(self):
        expected = {
            "ticket_yield", "fan_lifecycle", "campaign_lifecycle", "merchandising",
            "merch_pricing", "merch_bundle", "booking_opportunity", "outreach",
            "content_supply", "promotion_budget", "experimentation", "show_operations",
        }
        bootstrap = WORKER_BOOTSTRAP.read_text()
        migration = MIGRATION_TEXT
        model = (APP_ROOT / "autopilot/model.rs").read_text()
        for context in expected:
            self.assertIn(f"('{context}')", bootstrap)
            self.assertIn(f"'{context}'", migration)
            self.assertIn(f'"{context}"', model)


if __name__ == "__main__":
    unittest.main()
