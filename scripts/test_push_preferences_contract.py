from pathlib import Path
R=Path(__file__).resolve().parents[1]
m=(R/'migrations/0070_fan_push_preferences.sql').read_text()
p=(R/'crates/crowdrelay-api/src/push.rs').read_text()
w=(R/'crates/crowdrelay-worker/src/push_delivery/repository.rs').read_text()
assert all(x in m for x in ['fan_push_preferences','shows_enabled','quiet_start_minute','category'])
assert all(x in p for x in ['fan_preferences','update_fan_preferences','state.tenant.regional.timezone.clone()'])
assert 'quiet_timezone: String' in p
assert 'quiet_timezone: "Europe/Warsaw"' not in p
assert all(x in w for x in ['delivery.category = \'essential\'','preference.shows_enabled','quiet_hours_enabled','AT TIME ZONE $4'])
assert "AT TIME ZONE 'Europe/Warsaw'" not in w
worker=(R/'crates/crowdrelay-worker/src/push_delivery.rs').read_text()
assert 'CROWDRELAY_TENANT_TIMEZONE' in worker
assert 'PushDeliveryRepository::new' in worker
assert 'upsert_fan_push_preferences' in (R/'crates/crowdrelay-infra/src/push_preferences.rs').read_text()
print('PUSH_PREFERENCES_CONTRACT=PASS categories=6 quiet_hours=true essential_bypass=true timezone=tenant')

# Existing queued category deliveries must be terminally suppressed on opt-out;
# otherwise stale pushes can fire after a later re-enable. Quiet-hours delivery
# remains deferred by claim_due rather than terminally failed.
repo=(R/'crates/crowdrelay-worker/src/push_delivery/repository.rs').read_text()
assert "error_code = 'preference_disabled'" in repo
assert "delivery.status IN ('queued','retry_wait')" in repo
assert "delivery.category <> 'essential'" in repo
print('PUSH_PREFERENCE_SUPPRESSION=PASS')

ops=(R/'crates/crowdrelay-api/src/ops/query_support.rs').read_text()
metrics=(R/'crates/crowdrelay-api/src/lib.rs').read_text()
assert "error_code IS DISTINCT FROM 'preference_disabled'" in ops
assert "error_code = 'preference_disabled'" in ops
assert 'crowdrelay_push_delivery_suppressed' in metrics
print('PUSH_SUPPRESSION_OBSERVABILITY=PASS')

control=(R/'crates/crowdrelay-api/src/ecosystem/control_plane.rs').read_text()
reminders=(R/'crates/crowdrelay-worker/src/reminders.rs').read_text()
assert 'fan_push_deliveries_staff_category_check' in m
assert "SET category = 'staff'" in m
assert "source_kind, source_id, category, title" in control and "'show_checklist',\n                   inserted.id,\n                   'staff'," in control
assert "source_kind, source_id, category, title" in reminders and "'show_checklist',\n                   event.id,\n                   'staff'," in reminders
print('PUSH_STAFF_CATEGORY_INVARIANT=PASS')
