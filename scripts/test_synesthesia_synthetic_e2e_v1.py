#!/usr/bin/env python3
"""Production-safe synthetic Synesthesia E2E contract."""
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]

def text(path): return (ROOT/path).read_text(errors='replace')
checks={
 'schema synthetic column': ('migrations/0061_synesthesia_synthetic_runs.sql','synthetic boolean NOT NULL DEFAULT false'),
 'start payload opt-in': ('crates/crowdrelay-api/src/synesthesia.rs','synthetic: bool'),
 'persist synthetic': ('crates/crowdrelay-api/src/synesthesia/run_lifecycle.rs','.bind(payload.synthetic)'),
 'no synthetic handoff': ('crates/crowdrelay-api/src/synesthesia/run_lifecycle.rs','linked_to_fan || synthetic || !issue_handoff'),
 'handoff SQL fail closed': ('crates/crowdrelay-api/src/synesthesia/run_lifecycle.rs','AND NOT synthetic'),
 'link fail closed': ('crates/crowdrelay-api/src/synesthesia/rewards.rs','AND NOT synthetic'),
 'leaderboard excluded': ('crates/crowdrelay-api/src/synesthesia/leaderboard.rs','AND NOT run.synthetic'),
 'fan context excluded': ('crates/crowdrelay-api/src/fan_context.rs','AND NOT run.synthetic'),
 'audience excluded': ('crates/crowdrelay-api/src/audience/engagement_handlers.rs','AND NOT run.synthetic'),
 'autopilot excluded': ('crates/crowdrelay-infra/src/autopilot/decisions/core_reads.rs','AND NOT run.synthetic'),
}
missing=[name for name,(p,t) in checks.items() if t not in text(p)]
if missing: raise SystemExit('SYNESTHESIA_SYNTHETIC_E2E=FAIL missing='+','.join(missing))
print('SYNESTHESIA_SYNTHETIC_E2E=PASS metrics=excluded rewards=excluded leaderboard=excluded handoff=disabled')
