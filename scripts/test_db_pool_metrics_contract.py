#!/usr/bin/env python3
from pathlib import Path
s=(Path(__file__).resolve().parents[1]/'crates/crowdrelay-api/src/lib.rs').read_text()
for x in ['crowdrelay_db_pool_size','crowdrelay_db_pool_idle','crowdrelay_db_pool_in_use','crowdrelay_db_pool_max','crowdrelay_db_pool_utilization_ratio']: assert x in s
print('DB_POOL_METRICS_CONTRACT=PASS gauges=5')
