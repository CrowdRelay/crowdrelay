#!/usr/bin/env python3
import json
from pathlib import Path
from rust_source_tree import read_rust_module

root = Path(__file__).resolve().parents[1]
ecosystem = root.parent
tokens = json.loads((root / "integration/ecosystem/design-tokens.json").read_text())
errors = []

palette = tokens["palette"]
checks = {
    "design token version": tokens["version"] >= 2,
    "family background": palette["background"] == "#070908",
    "family surface": palette["surface"] == "#101715",
    "signal accent": palette["signal"] == "#84b4ac",
    "signal hot": palette["signalHot"] == "#93c6c0",
    "signal deep": palette["signalDeep"] == "#26655d",
    "warning remains amber": palette["warning"] == "#f3c51a",
    "micro floor": tokens["type"]["microMinPx"] >= 12,
    "touch floor": tokens["controls"]["minimumTouchPx"] >= 44,
    "radius ceiling": tokens["controls"]["radiusPx"] <= 6,
    "distinct products": set(tokens["products"]) == {"virya", "virya-signal", "synesthesia", "crowdrelay"},
}

signal = ecosystem / "virya-signal"
virya = ecosystem / "virya"
synesthesia = ecosystem / "synesthesia"
if signal.is_dir() and virya.is_dir() and synesthesia.is_dir():
    signal_css = (signal / "styles.css").read_text()
    virya_css = (virya / "src/styles/global.css").read_text()
    syn_hud = (synesthesia / "scripts/ui/app_hud.gd").read_text()
    fan_home = read_rust_module(ecosystem, "virya-signal/src/app/fan.rs")
    fan_home_overview = (signal / "src/app/fan_home.rs").read_text()
    fan_formatters = (signal / "src/app/formatters.rs").read_text()

    signal_v2_tokens = (
        "--bg: #070908",
        "--surface: #101715",
        "--signal: #84b4ac",
        "--signal-hot: #93c6c0",
        "--signal-deep: #26655d",
        "--warning: #f3c51a",
        "--control-radius: 6px",
    )
    virya_v2_tokens = (
        "--virya-bg: #070908",
        "--virya-surface: #101715",
        "--virya-signal: #84b4ac",
        "--virya-signal-hot: #93c6c0",
        "--virya-signal-deep: #26655d",
        "--virya-warning: #f3c51a",
    )
    checks.update({
        "signal V2 token adoption": all(token in signal_css for token in signal_v2_tokens),
        "virya V2 token adoption": all(token in virya_css for token in virya_v2_tokens),
        "legacy yellow is not Signal primary": "--signal: #f3c51a" not in signal_css and "--virya-signal: #f3c51a" not in virya_css,
        "synesthesia art-first HUD": all(token in syn_hud for token in ("enter_completion_beat", "subtitle_label.visible = false", "palette_row.visible = false", "brush_label.visible = false")),
        "participation without XP": "synesthesia-home-card" in fan_home_overview and "completed_runs" in fan_formatters and all(word not in (fan_home + fan_home_overview + fan_formatters).lower() for word in ("experience points", " xp ", "level-up", "streak")),
    })

for label, ok in checks.items():
    if not ok:
        errors.append(label)
if errors:
    print("ECOSYSTEM_DESIGN_CONTRACT=FAIL " + ", ".join(errors))
    raise SystemExit(1)
print(
    f"ECOSYSTEM_DESIGN_CONTRACT=PASS checks={len(checks)} "
    "family=v2-clinical-mint products=distinct radius<=6"
)
