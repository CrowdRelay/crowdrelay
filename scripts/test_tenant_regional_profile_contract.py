#!/usr/bin/env python3
from pathlib import Path
R=Path(__file__).resolve().parents[1]; t=(R/"crates/crowdrelay-api/src/tenant.rs").read_text(); r=(R/"crates/crowdrelay-api/src/routing.rs").read_text(); m=(R/"crates/crowdrelay-api/src/meta.rs").read_text(); p=(R/"crates/crowdrelay-api/src/push.rs").read_text(); w=(R/"crates/crowdrelay-worker/src/push_delivery.rs").read_text(); q=(R/"crates/crowdrelay-worker/src/push_delivery/repository.rs").read_text()
checks={"endpoint":"/v1/public/tenant/config" in r,"cap":"tenant_regional_profile_v1" in m,"tz":"CROWDRELAY_TENANT_TIMEZONE" in t,"currency":"CROWDRELAY_TENANT_CURRENCY" in t,"residency":"CROWDRELAY_TENANT_DATA_REGION" in t,"provenance":"RegionalSource" in t,"push":"state.tenant.regional.timezone.clone()" in p,"worker":"CROWDRELAY_TENANT_TIMEZONE" in w,"sql":"AT TIME ZONE $4" in q,"no-warsaw-sql":"AT TIME ZONE 'Europe/Warsaw'" not in q}
bad=[k for k,v in checks.items() if not v]
if bad: raise SystemExit("TENANT_REGIONAL_PROFILE_CONTRACT=FAIL "+','.join(bad))
print("TENANT_REGIONAL_PROFILE_CONTRACT=PASS push-timezone=tenant no-runtime-inference=true")
