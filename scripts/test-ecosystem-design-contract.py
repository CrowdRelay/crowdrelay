#!/usr/bin/env python3
import json
from pathlib import Path

root = Path(__file__).resolve().parents[1]
ecosystem = root.parent
tokens = json.loads((root / "integration/ecosystem/design-tokens.json").read_text())
errors = []
signal_css = (ecosystem / "virya-signal/styles.css").read_text()
virya_css = (ecosystem / "virya/src/styles/global.css").read_text()
syn_hud = (ecosystem / "synesthesia/scripts/ui/app_hud.gd").read_text()
fan_home = (ecosystem / "virya-signal/src/app/fan_home.rs").read_text()

checks = {
    "signal accent": tokens["palette"]["signal"] in signal_css,
    "signal micro floor": f'--type-micro: {tokens["type"]["microMinPx"]}px' in signal_css,
    "signal touch floor": f'--touch-min: {tokens["controls"]["minimumTouchPx"]}px' in signal_css,
    "virya semantic accent": '--virya-signal: #f3c51a' in virya_css,
    "virya touch contract": '--virya-touch-min: 44px' in virya_css,
    "synesthesia art-first HUD": all(token in syn_hud for token in ('enter_completion_beat', 'subtitle_label.visible = false', 'palette_row.visible = false', 'brush_label.visible = false')),
    "participation history": 'participation-history' in fan_home,
    "no XP gamification": all(word not in fan_home.lower() for word in ('experience points', ' xp ', 'level-up', 'streak')),
}
for label, ok in checks.items():
    if not ok:
        errors.append(label)
if errors:
    print("ECOSYSTEM_DESIGN_CONTRACT=FAIL " + ", ".join(errors))
    raise SystemExit(1)
print(f"ECOSYSTEM_DESIGN_CONTRACT=PASS checks={len(checks)} family=shared-semantics products=distinct")
