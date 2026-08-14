#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
PARENT_MAX = 1000
CHUNK_MAX = 1000

CONTRACT = {
    "crates/crowdrelay-infra/tests/acquisition_postgres.rs": [
        "acquisition_postgres/helpers.rs",
    ],
    "crates/crowdrelay-api/src/audience.rs": [
        "audience/models.rs",
        "audience/engagement_handlers.rs",
        "audience/delivery_handlers.rs",
        "audience/query_support.rs",
    ],
    "crates/crowdrelay-api/src/ecosystem.rs": ["ecosystem/control_plane.rs"],
    "crates/crowdrelay-api/src/proofs.rs": [
        "proofs/admin_and_public.rs",
        "proofs/read_support.rs",
        "proofs/relayer.rs",
        "proofs/support.rs",
    ],
    "crates/crowdrelay-worker/src/event_sync.rs": [
        "event_sync/persistence.rs",
        "event_sync/announcements.rs",
    ],
    "crates/crowdrelay-worker/src/draws.rs": [
        "draws/execution.rs",
        "draws/candidates_and_rewards.rs",
    ],
    "crates/crowdrelay-infra/src/acquisition.rs": [
        "acquisition/ingress_methods.rs",
        "acquisition/persistence_methods.rs",
    ],
    "crates/crowdrelay-infra/src/referrals.rs": [
        "referrals/repository.rs",
        "referrals/reward_lifecycle.rs",
    ],
    "crates/crowdrelay-infra/src/events.rs": [
        "events/buffer.rs",
        "events/support.rs",
        "events/tests.rs",
    ],
    "crates/crowdrelay-infra/src/admission.rs": [
        "admission/issue_claim.rs",
        "admission/pass_lifecycle.rs",
        "admission/support.rs",
    ],
    "crates/crowdrelay-infra/src/autopilot.rs": [
        "autopilot/mapping.rs",
        "autopilot/execution.rs",
        "autopilot/support.rs",
    ],
    "crates/crowdrelay-infra/src/autopilot/decisions.rs": [
        "decisions/core_reads.rs",
        "decisions/opportunity_reads.rs",
        "decisions/persist.rs",
    ],
    "crates/crowdrelay-api/src/commerce/campaigns.rs": [
        "campaigns/recommendations.rs",
        "campaigns/reward_campaigns.rs",
        "campaigns/draws.rs",
        "campaigns/fulfillments.rs",
        "campaigns/reservations.rs",
    ],
    "crates/crowdrelay-api/src/commerce/inventory.rs": [
        "inventory/catalog.rs",
        "inventory/stocktake.rs",
        "inventory/reservations.rs",
    ],
    "crates/crowdrelay-api/src/accounting.rs": [
        "accounting/models.rs",
        "accounting/handlers.rs",
        "accounting/core.rs",
        "accounting/csv_support.rs",
    ],
    "crates/crowdrelay-api/src/ops.rs": [
        "ops/models.rs",
        "ops/handlers.rs",
        "ops/query_support.rs",
    ],
    "crates/crowdrelay-api/src/autopilot.rs": [
        "autopilot/authority_booking.rs",
        "autopilot/promotion_market.rs",
        "autopilot/outreach_release.rs",
        "autopilot/experiments_actions.rs",
        "autopilot/validation.rs",
    ],
    "crates/crowdrelay-application/src/autopilot/control.rs": [
        "control/state_ports.rs",
        "control/runtime_ports.rs",
    ],
    "crates/crowdrelay-infra/src/config.rs": [
        "config/parsing.rs",
        "config/tests.rs",
    ],
    "crates/crowdrelay-worker/src/retention.rs": [
        "retention/steps.rs",
        "retention/tests.rs",
    ],
    "crates/crowdrelay-api/src/synesthesia.rs": [
        "synesthesia/run_lifecycle.rs",
        "synesthesia/rewards.rs",
        "synesthesia/validation.rs",
    ],
    "crates/crowdrelay-api/src/ticketing/read_model.rs": [
        "read_model/sale.rs",
        "read_model/orders.rs",
        "read_model/views.rs",
    ],
    "crates/crowdrelay-application/src/autopilot/evaluate.rs": [
        "evaluate/candidates.rs",
        "evaluate/tests.rs",
    ],
}


def loc(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def fail(message: str) -> None:
    raise SystemExit(f"MODULARITY_CONTRACT=FAIL {message}")



_INCLUDE = re.compile(r'include!\("([^"]+)"\);')

def validate_include_tree(path: Path, seen: set[Path] | None = None) -> None:
    seen = set() if seen is None else seen
    resolved = path.resolve()
    if resolved in seen:
        return
    seen.add(resolved)
    for rel in _INCLUDE.findall(path.read_text(encoding="utf-8")):
        child = path.parent / rel
        if not child.is_file():
            fail(f"broken-include={path.relative_to(ROOT)}:{rel}")
        validate_include_tree(child, seen)

def main() -> None:
    chunks = 0
    for parent_rel, child_rels in CONTRACT.items():
        parent = ROOT / parent_rel
        if not parent.is_file():
            fail(f"missing-parent={parent_rel}")
        parent_loc = loc(parent)
        if parent_loc > PARENT_MAX:
            fail(f"parent-too-large={parent_rel} loc={parent_loc} max={PARENT_MAX}")

        source = parent.read_text(encoding="utf-8")
        validate_include_tree(parent)
        for child_rel in child_rels:
            child = parent.parent / child_rel
            if not child.is_file():
                fail(f"missing-chunk={child.relative_to(ROOT)}")
            child_loc = loc(child)
            if child_loc > CHUNK_MAX:
                fail(
                    f"chunk-too-large={child.relative_to(ROOT)} "
                    f"loc={child_loc} max={CHUNK_MAX}"
                )
            include_expr = f'include!("{child_rel}");'
            if include_expr not in source:
                fail(f"missing-include={parent_rel}:{child_rel}")
            chunks += 1

    print(
        "MODULARITY_CONTRACT=PASS "
        f"parents={len(CONTRACT)} chunks={chunks} "
        f"parent_max={PARENT_MAX} chunk_max={CHUNK_MAX}"
    )


if __name__ == "__main__":
    main()
