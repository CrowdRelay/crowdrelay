from __future__ import annotations
import json
from pathlib import Path
ROOT=Path(__file__).resolve().parents[1]
FILES=['n8n/examples/autopilot-spotify-growth.example.json','n8n/examples/autopilot-bandsintown-growth.example.json','n8n/examples/autopilot-growth-route-executor.example.json','n8n/examples/autopilot-growth-daily-kpi.example.json']
for rel in FILES:
    data=json.loads((ROOT/rel).read_text(encoding='utf-8'))
    assert data['active'] is False
    assert data['settings']['saveDataErrorExecution']=='none'
    assert data['settings']['saveDataSuccessExecution']=='none'
    assert data['settings']['saveManualExecutions'] is False
print('autopilot growth bundle: PASS')
