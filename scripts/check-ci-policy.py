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

# A scheduled security workflow is useful only if dependency changes also exercise it.
security_workflow = workflow_dir / "security.yml"
if not security_workflow.exists():
    failures.append(".github/workflows/security.yml: standalone dependency-security workflow is required")
else:
    security_text = security_workflow.read_text()
    for trigger in ("push", "pull_request", "schedule", "workflow_dispatch"):
        if not re.search(rf"(?m)^  {re.escape(trigger)}:\s*$", security_text):
            failures.append(f".github/workflows/security.yml: missing {trigger} trigger")
    if "Cargo.lock" not in security_text:
        failures.append(".github/workflows/security.yml: dependency lockfile must trigger the audit")
    if "cargo install cargo-audit --locked --version 0.22.2" not in security_text:
        failures.append(".github/workflows/security.yml: pinned/controlled dependency audit command is required")
    if "continue-on-error: true" in security_text:
        failures.append(".github/workflows/security.yml: dependency audit must fail closed")
    if "github.ref" not in security_text and "concurrency:" in security_text:
        failures.append(".github/workflows/security.yml: concurrency must not collapse unrelated refs")

if failures:
    for failure in failures:
        print(f"CI_POLICY=FAIL {failure}", file=sys.stderr)
    raise SystemExit(1)
print("CI_POLICY=PASS actions=sha-pinned netlify=source-build-disabled")
