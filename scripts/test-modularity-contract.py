#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PARENT_MAX = 1200
CHUNK_MAX = 1200

CONTRACT = {
    "crates/crowdrelay-api/src/audience.rs": ["audience/engagement_handlers.rs"],
    "crates/crowdrelay-api/src/ecosystem.rs": ["ecosystem/control_plane.rs"],
    "crates/crowdrelay-api/src/proofs.rs": [
        "proofs/admin_and_public.rs",
        "proofs/relayer.rs",
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
    "crates/crowdrelay-infra/src/autopilot.rs": ["autopilot/execution.rs"],
}


def loc(path: Path) -> int:
    return len(path.read_text(encoding="utf-8").splitlines())


def fail(message: str) -> None:
    raise SystemExit(f"MODULARITY_CONTRACT=FAIL {message}")


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
