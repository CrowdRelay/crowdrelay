from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FILES = [
    "n8n/examples/autopilot-spotify-growth.example.json",
    "n8n/examples/autopilot-bandsintown-growth.example.json",
    "n8n/examples/autopilot-growth-route-executor.example.json",
    "n8n/examples/autopilot-growth-daily-kpi.example.json",
]

for relative in FILES:
    path = ROOT / relative
    data = json.loads(path.read_text(encoding="utf-8"))
    assert data["active"] is False
    assert data["settings"]["saveDataErrorExecution"] == "none"
    assert data["settings"]["saveDataSuccessExecution"] == "none"
    assert data["settings"]["saveManualExecutions"] is False
    assert any(node.get("name") == "Report Spotify growth receipt" or node.get("name") == "Report Bandsintown growth receipt" or node.get("name") == "Report route receipt" for node in data["nodes"])

print("autopilot growth bundle: PASS")
