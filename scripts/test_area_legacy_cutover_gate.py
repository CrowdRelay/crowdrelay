#!/usr/bin/env python3
from pathlib import Path
R=Path(__file__).resolve().parents[1]
eco=(R/'crates/crowdrelay-api/src/ecosystem.rs').read_text()
claims=(R/'crates/crowdrelay-api/src/area/endpoints.rs').read_text()
wallet=(R/'crates/crowdrelay-api/src/area/legacy_wallet.rs').read_text()
checks=[
 '("area_legacy_imports_enabled", true)' in eco,
 'AREA_LEGACY_IMPORTS_DISABLED' in claims and 'feature_enabled(&state, "area_legacy_imports_enabled")' in claims,
 'AREA_LEGACY_IMPORTS_DISABLED' in wallet and 'feature_enabled(&state, "area_legacy_imports_enabled")' in wallet,
]
for i,x in enumerate(checks,1): print(f'AREA_CUTOVER_GATE_{i}={"PASS" if x else "FAIL"}')
if not all(checks): raise SystemExit(1)
print('AREA_LEGACY_CUTOVER_GATE=PASS checks=3 default=on fail_closed=true')

metrics=(R/'crates/crowdrelay-api/src/lib.rs').read_text()
http=(R/'crates/crowdrelay-api/src/http_metrics.rs').read_text()
claims=(R/'crates/crowdrelay-api/src/area/endpoints.rs').read_text()
wallet=(R/'crates/crowdrelay-api/src/area/legacy_wallet.rs').read_text()
assert 'crowdrelay_legacy_area_claim_import_attempt_total' in metrics
assert 'crowdrelay_legacy_area_wallet_import_attempt_total' in metrics
assert 'record_legacy_area_claim_import_attempt' in http
assert claims.index('record_legacy_area_claim_import_attempt') < claims.index('record_legacy_area_claim_import()')
assert wallet.index('record_legacy_area_wallet_import_attempt') < wallet.index('record_legacy_area_wallet_import()')
print('AREA_CUTOVER_APPLIED_COUNTERS=PASS')
