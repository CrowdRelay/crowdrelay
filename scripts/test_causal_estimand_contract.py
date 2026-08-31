#!/usr/bin/env python3
"""Source-level contract for the active causal estimand in the production learner.

The production learner (growth_intelligence.rs) must explicitly select
IntentToTreat as the active estimand. PerProtocol is a legitimate domain
concept and remains available in the enum, but it is NOT the current
production methodology. Switching to PerProtocol requires an explicit code
change visible in review — it must not happen silently via config or
data-derived selection.

This contract fails if:
  - growth_intelligence.rs does not contain an explicit ITT selection
  - growth_intelligence.rs contains an active PerProtocol selection
  - The estimand is derived from data/config rather than hardcoded
"""
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
GI_PATH = ROOT / "crates/crowdrelay-infra/src/autopilot/operations/growth_intelligence.rs"
EXPERIMENT_PATH = ROOT / "crates/crowdrelay-brain/src/experiment.rs"

errors = []

if not GI_PATH.exists():
    errors.append(f"missing {GI_PATH.relative_to(ROOT)}")
    raise SystemExit(1)

gi = GI_PATH.read_text()

# 1. The production learner must explicitly select IntentToTreat.
if "CausalEstimand::IntentToTreat" not in gi:
    errors.append(
        "growth_intelligence.rs: missing explicit CausalEstimand::IntentToTreat selection"
    )

# 2. The active estimand assignment must use ITT, not PerProtocol.
#    Check every line that assigns the estimand variable.
estimand_lines = [
    line for line in gi.splitlines()
    if "let estimand" in line and "CausalEstimand" in line
]
if not estimand_lines:
    errors.append(
        "growth_intelligence.rs: no explicit estimand selection found "
        "(expected 'let estimand = CausalEstimand::IntentToTreat')"
    )
else:
    has_itt = any("IntentToTreat" in line for line in estimand_lines)
    has_per_protocol = any("PerProtocol" in line for line in estimand_lines)
    if not has_itt:
        errors.append(
            "growth_intelligence.rs: active estimand must be IntentToTreat, "
            "found no ITT selection"
        )
    if has_per_protocol:
        errors.append(
            "growth_intelligence.rs: active estimand must NOT be PerProtocol — "
            "switching requires explicit review and contract update"
        )

# 3. The estimand must not be derived from data or config.
#    It must be a hardcoded selection, not read from DB/env/config.
for forbidden in (
    "estimand = serde_json",
    "estimand = config",
    "estimand = row",
    "estimand = env",
    "estimand: CausalEstimand",
):
    if forbidden in gi:
        errors.append(
            f"growth_intelligence.rs: estimand must be hardcoded, "
            f"found dynamic selection pattern: {forbidden!r}"
        )

# 4. The CausalEstimand enum must still define both variants.
exp = EXPERIMENT_PATH.read_text()
for marker in ("IntentToTreat", "PerProtocol", "includes_in_treatment_effect"):
    if marker not in exp:
        errors.append(f"experiment.rs: missing marker {marker!r}")

# 5. PerProtocol must document that it is a population filter, not a
#    fully identified estimator.
if "identification assumptions" not in exp.lower():
    errors.append(
        "experiment.rs: PerProtocol must document its identification assumptions"
    )

if errors:
    print("CAUSAL_ESTIMAND_CONTRACT=FAIL", file=sys.stderr)
    for error in errors:
        print(f"  - {error}", file=sys.stderr)
    raise SystemExit(1)
print("CAUSAL_ESTIMAND_CONTRACT=PASS checks=5")
