#!/usr/bin/env python3
"""Fail closed on mutable GitHub Actions refs and Netlify source builds."""
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
HEX40 = re.compile(r"^[0-9a-f]{40}$")
USES = re.compile(r"^\s*-?\s*uses:\s*([^\s#]+)", re.MULTILINE)
failures: list[str] = []

workflow_dir = ROOT / ".github" / "workflows"
if workflow_dir.exists():
    for path in sorted(workflow_dir.glob("*.y*ml")):
        text = path.read_text()
        if not re.search(r"(?m)^permissions:\s*$", text):
            failures.append(f"{path.relative_to(ROOT)}: workflow permissions must be explicit")
        elif not re.search(r"(?m)^  contents:\s+(read|write)\s*$", text):
            failures.append(f"{path.relative_to(ROOT)}: top-level contents permission must be explicit")
        for ref in USES.findall(text):
            # Local and Docker actions are not Git refs managed by this policy.
            if ref.startswith("./") or ref.startswith("docker://"):
                continue
            if "@" not in ref:
                failures.append(f"{path.relative_to(ROOT)}: action has no @ref: {ref}")
                continue
            _, version = ref.rsplit("@", 1)
            if not HEX40.fullmatch(version):
                failures.append(
                    f"{path.relative_to(ROOT)}: mutable action ref forbidden: {ref}"
                )

netlify = ROOT / "netlify.toml"
if netlify.exists():
    text = netlify.read_text()
    if 'ignore = "exit 0"' not in text:
        failures.append("netlify.toml: linked source builds must be skipped")
    if re.search(r"(?m)^\s*command\s*=", text) or "[[plugins]]" in text:
        failures.append("netlify.toml: source build command/plugin is forbidden")
    deploy_workflows = "\n".join(
        p.read_text() for p in workflow_dir.glob("*.y*ml")
    ) if workflow_dir.exists() else ""
    if "netlify-cli" in deploy_workflows and "--no-build" not in deploy_workflows:
        failures.append("Netlify deploy workflow must pass --no-build")

if failures:
    for failure in failures:
        print(f"CI_POLICY=FAIL {failure}", file=sys.stderr)
    raise SystemExit(1)
print("CI_POLICY=PASS actions=sha-pinned netlify=source-build-disabled")
