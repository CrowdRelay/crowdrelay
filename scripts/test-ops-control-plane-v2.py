#!/usr/bin/env python3
from pathlib import Path

root = Path(__file__).resolve().parents[1]
ops = (root / "crates/crowdrelay-api/src/ops_timeline.rs").read_text()
router = (root / "crates/crowdrelay-api/src/ops_routes.rs").read_text()
migration = (root / "migrations/0037_ops_request_timeline_indexes.sql").read_text()
openapi = (root / "openapi/openapi.yaml").read_text()
errors = []

def require(ok: bool, message: str) -> None:
    if not ok:
        errors.append(message)

require('/v1/admin/ops/operations/{request_id}' in router, 'missing admin timeline route')
for source in ("audit_events", "outbox_events", "webhook_deliveries", "operator_actions"):
    require(source in ops, f'missing timeline source {source}')
for forbidden in ('payload AS', 'metadata AS', 'details AS', 'endpoint.url', 'signing_secret_ref'):
    require(forbidden not in ops, f'timeline leaks/selects forbidden field {forbidden}')
require('LIMIT 250' in ops, 'timeline must be bounded')
require('value.len() <= 128' in ops, 'request id must be bounded')
for index in ('audit_events_ops_request_timeline_idx', 'outbox_events_ops_request_timeline_idx', 'operator_actions_ops_request_timeline_idx'):
    require(index in migration, f'missing request timeline index {index}')
require('/admin/ops/operations/{request_id}:' in openapi, 'timeline missing from OpenAPI')
if errors:
    print('OPS_CONTROL_PLANE_V2=FAIL')
    for error in errors:
        print('-', error)
    raise SystemExit(1)
print('OPS_CONTROL_PLANE_V2=PASS sources=4 payloads=excluded indexes=3')
