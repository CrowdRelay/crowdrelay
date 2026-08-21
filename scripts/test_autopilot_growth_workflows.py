from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = [
    ROOT / "n8n/examples/autopilot-spotify-growth.example.json",
    ROOT / "n8n/examples/autopilot-bandsintown-growth.example.json",
]


def main() -> None:
    for path in WORKFLOWS:
        data = json.loads(path.read_text(encoding="utf-8"))
        assert data["active"] is False
        assert data["settings"]["saveDataErrorExecution"] == "none"
        assert data["settings"]["saveDataSuccessExecution"] == "none"
        assert data["settings"]["saveManualExecutions"] is False
        names = {node["name"] for node in data["nodes"]}
        assert "Claim action once" in names
        assert any("growth receipt" in name.lower() for name in names)
        assert any("Report" in name and "receipt" in name for name in names)
        text = path.read_text(encoding="utf-8")
        assert "execution-claim" in text
        assert "execution-report" in text
        assert "CROWDRELAY_ADMIN_TOKEN" in text
    print("autopilot growth workflow contracts: PASS")


if __name__ == "__main__":
    main()
