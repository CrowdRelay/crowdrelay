from pathlib import Path
R=Path(__file__).resolve().parents[1]
checks={
 'migration': all(x in (R/'migrations/0069_merch_order_attribution.sql').read_text() for x in ['merch_order_facts','event_pickup','inventory_reservation_id']),
 'infra': all(x in (R/'crates/crowdrelay-infra/src/commerce.rs').read_text() for x in ['record_confirmed_merch_order',"status = 'committed'",'event_merch_summary','pickup_items']),
 'api_delegates': 'crowdrelay_infra::commerce::record_confirmed_merch_order' in (R/'crates/crowdrelay-api/src/commerce.rs').read_text(),
 'routes': all(x in (R/'crates/crowdrelay-api/src/routing.rs').read_text() for x in ['/v1/internal/merch/orders/confirmed','/v1/staff/events/{event_id}/commerce-summary']),
}
for k,v in checks.items(): assert v,k
print(f"MERCH_EVENT_ATTRIBUTION_CONTRACT=PASS checks={len(checks)}")

# fan_id is late enrichment, not immutable Stripe identity. A webhook replay
# after fan signup must remain idempotent and may fill NULL -> fan_id.
infra=(R/'crates/crowdrelay-infra/src/commerce.rs').read_text()
assert 'existing.fan_id == fan_id' not in infra
assert 'AND fan_id IS NULL' in infra
assert 'enrich merch order fan failed' in infra
print('MERCH_FAN_ENRICHMENT_IDEMPOTENCY=PASS')

migration=(R/'migrations/0069_merch_order_attribution.sql').read_text()
assert 'merch_order_facts_fan_idx' in migration
assert '(workspace_id, fan_id, confirmed_at DESC, id)' in migration
print('MERCH_FAN_JOURNEY_INDEX=PASS')

same_fact = infra.split('fn same_fact(', 1)[1].split('\n}', 1)[0]
assert 'confirmed_at' not in same_fact
print('MERCH_CONFIRMATION_TIME_NOT_IDENTITY=PASS')
