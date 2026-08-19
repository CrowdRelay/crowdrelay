#!/usr/bin/env python3
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
spec=(ROOT/'crates/crowdrelay-worker/src/bootstrap/specifications.rs').read_text()
boot=(ROOT/'crates/crowdrelay-worker/src/bootstrap.rs').read_text()
persist=(ROOT/'crates/crowdrelay-worker/src/bootstrap/persistence.rs').read_text()
checks=[
 'smart_links: Vec<RawSmartLinkSpec>' in spec,
 'smart_links: Vec<SmartLinkSpec>' in boot and 'smart_links[].destination_url' in boot,
 'workspace_id, None, smart_link' in persist and 'campaign_id: Option<Uuid>' in persist,
]
for i,v in enumerate(checks,1): print(f'WORKSPACE_SMART_LINK_{i}={"PASS" if v else "FAIL"}')
if not all(checks): raise SystemExit(1)
print('WORKSPACE_SMART_LINKS=PASS checks=3')
