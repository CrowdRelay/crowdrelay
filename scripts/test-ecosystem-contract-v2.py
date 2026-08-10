#!/usr/bin/env python3
"""Cross-repository static compatibility contract for an ecosystem checkout."""
from __future__ import annotations
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
ecosystem = root.parent
manifest = json.loads((root / "integration/ecosystem/compatibility.json").read_text())
meta = (root / "crates/crowdrelay-api/src/meta.rs").read_text()
router = (root / "crates/crowdrelay-api/src/lib.rs").read_text()
openapi = (root / "openapi/openapi.yaml").read_text()
errors: list[str] = []

for capability in manifest["requiredCapabilities"]:
    if capability not in meta:
        errors.append(f"backend /meta missing capability: {capability}")
for path in manifest["readOnlyProductionSmokePaths"]:
    if path == "/v1/health/live" or path == "/v1/health/ready":
        if path not in router:
            errors.append(f"router missing smoke path: {path}")
    else:
        spec_path = path.removeprefix("/v1")
        if spec_path not in openapi:
            errors.append(f"OpenAPI missing smoke path: {path}")

virya = ecosystem / "virya"
if virya.exists():
    client = (virya / "src/lib/crowdrelay-client.ts").read_text()
    area = (virya / "src/server/crowdrelayArea.ts").read_text()
    for marker in ["@generated-contract openapi-sha256:", "SynesthesiaLinkResult", "TicketWallet"]:
        if marker not in client:
            errors.append(f"Virya client missing canonical contract marker: {marker}")
    for marker in ["importLegacyAreaWallet", "createAreaBackendVoucher", "reserveAreaBackendTicketReward"]:
        if marker not in area:
            errors.append(f"Virya AREA handoff missing: {marker}")

signal = ecosystem / "virya-signal"
if signal.exists():
    native = "\n".join(
        p.read_text() for p in [
            signal / "src-tauri/src/api/client.rs",
            signal / "src-tauri/src/api/fan.rs",
            signal / "src-tauri/src/api/public.rs",
            signal / "src-tauri/src/api/ticketing.rs",
        ]
    )
    for capability in ["signal_fan_context_v1", "area_wallet_postgres_v2", "ticketing_v1"]:
        if capability not in native:
            errors.append(f"Signal does not gate capability: {capability}")
    if '"meta"' not in native or "ecosystem_meta" not in native:
        errors.append("Signal does not consume /v1/meta")

syn = ecosystem / "synesthesia"
if syn.exists():
    reward = (syn / "scripts/reward_client.gd").read_text()
    if "synesthesia" not in reward.lower():
        errors.append("Synesthesia reward client lost CrowdRelay run contract")
    headers = (syn / "web/_headers").read_text()
    if "https://signal-api.virya.music" not in headers:
        errors.append("Synesthesia CSP no longer permits canonical CrowdRelay API")

if errors:
    print("ECOSYSTEM_CONTRACT_V2=FAIL")
    for error in errors:
        print(f"- {error}")
    raise SystemExit(1)
print(
    "ECOSYSTEM_CONTRACT_V2=PASS "
    f"schema>={manifest['minimumSchemaVersion']} capabilities={len(manifest['requiredCapabilities'])}"
)
