from pathlib import Path
R=Path(__file__).resolve().parents[1]
source=(R/'crates/crowdrelay-api/src/audience/engagement_handlers.rs').read_text()
route=(R/'crates/crowdrelay-api/src/routing.rs').read_text()
for token in ['fan_acquisition_events','event_interests','ticket_orders','admission_passes','synesthesia_runs','area_claims','merch_order_facts','LIMIT 200']:
    assert token in source,token
assert '/v1/admin/audience/fans/{fan_id}/journey' in route
assert 'ORDER BY occurred_at DESC' in source
print('FAN_JOURNEY_CONTRACT=PASS sources=7 bounded=200')
