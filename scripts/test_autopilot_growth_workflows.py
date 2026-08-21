from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = [
    ROOT / "n8n/examples/autopilot-outreach-executor.example.json",
    ROOT / "n8n/examples/autopilot-outreach-reply-monitor.example.json",
    ROOT / "n8n/examples/autopilot-spotify-growth.example.json",
    ROOT / "n8n/examples/autopilot-bandsintown-growth.example.json",
    ROOT / "n8n/examples/autopilot-growth-route-executor.example.json",
    ROOT / "n8n/examples/autopilot-growth-daily-kpi.example.json",
]


def main() -> None:
    for path in WORKFLOWS:
        data = json.loads(path.read_text(encoding="utf-8"))
        assert data["active"] is False, path
        settings = data["settings"]
        assert settings["saveDataErrorExecution"] == "none", path
        assert settings["saveDataSuccessExecution"] == "none", path
        assert settings["saveManualExecutions"] is False, path
        names = {node["name"] for node in data["nodes"]}
        text = path.read_text(encoding="utf-8")
        assert "CROWDRELAY" in text, path
        if "growth" in path.name or "outreach-executor" in path.name:
            assert "execution-claim" in text, path
            assert "execution-report" in text, path
            assert "Claim action once" in names or "Claim eligible action" in names, path
        if "spotify-growth" in path.name:
            assert "Fail-closed claim gate" in names, path
            assert "Campaign creation gate" in names, path
            assert "Read Spotify artist state" in names, path
        if "bandsintown-growth" in path.name:
            assert "Fail-closed claim gate" in names, path
            assert "Campaign creation gate" in names, path
            assert "Read Bandsintown artist state" in names, path
            assert "Read Bandsintown upcoming events" in names, path
            assert "/events'" in text, path
        if "growth-route-executor" in path.name:
            assert "Fail-closed claim gate" in names, path
            assert "VIRYA_GROWTH_ROUTE_MANIFEST_JSON" in text, path
            assert "idempotent" in text, path
        if "growth-daily-kpi" in path.name:
            assert "Wait for all KPI reads" in names, path
            assert "n8n-nodes-base.merge" in text, path
    print("autopilot growth workflow contracts: PASS")


if __name__ == "__main__":
    main()
