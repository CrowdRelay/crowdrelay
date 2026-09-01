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

# Per-change dependency security is a reusable-workflow call inside CI, so
# image publication cannot observe CI=PASS while RustSec is red. The called
# security.yml owns the single copy of the audit and the weekly/manual
# freshness triggers.
ci_workflow = workflow_dir / "ci.yml"
if not ci_workflow.exists():
    failures.append(".github/workflows/ci.yml: canonical CI workflow is required")
else:
    ci_text = ci_workflow.read_text()
    for contract in (
        "dependency-security:",
        "uses: ./.github/workflows/security.yml",
    ):
        if contract not in ci_text:
            failures.append(f".github/workflows/ci.yml: dependency-security contract missing: {contract}")
    # The container gate must exercise every platform that is published for
    # production, each on its own native runner. Dropping a platform here lets
    # a pull request go green while breaking the host that runs it.
    containers_marker = "  containers:\n"
    if containers_marker not in ci_text:
        failures.append(".github/workflows/ci.yml: containers job is required")
    else:
        containers_text = ci_text.split(containers_marker, 1)[1]
        if "--set '*.platform=${{ matrix.platform }}'" not in containers_text:
            failures.append(
                ".github/workflows/ci.yml: container gate must build the matrix platform"
            )
        for platform in ("linux/amd64", "linux/arm64"):
            if f"platform: {platform}\n" not in containers_text:
                failures.append(
                    f".github/workflows/ci.yml: container gate must cover {platform}"
                )
        for runner in ("ubuntu-24.04", "ubuntu-24.04-arm"):
            if f"runner: {runner}\n" not in containers_text:
                failures.append(
                    f".github/workflows/ci.yml: container gate must run natively on {runner}"
                )
        if "setup-qemu-action" in containers_text:
            failures.append(
                ".github/workflows/ci.yml: emulated container gate forbidden; use native runners"
            )

security_workflow = workflow_dir / "security.yml"
if not security_workflow.exists():
    failures.append(".github/workflows/security.yml: standalone dependency-security workflow is required")
else:
    security_text = security_workflow.read_text()
    for trigger in ("schedule", "workflow_dispatch"):
        if not re.search(rf"(?m)^  {re.escape(trigger)}:\s*$", security_text):
            failures.append(f".github/workflows/security.yml: missing {trigger} trigger")
    for duplicate_trigger in ("push", "pull_request"):
        if re.search(rf"(?m)^  {re.escape(duplicate_trigger)}:\s*$", security_text):
            failures.append(
                f".github/workflows/security.yml: {duplicate_trigger} duplicates CI dependency-security"
            )
    if "tool: cargo-audit@0.22.2" not in security_text:
        failures.append(".github/workflows/security.yml: pinned prebuilt cargo-audit install is required")
    if "cargo install cargo-audit" in security_text:
        failures.append(
            ".github/workflows/security.yml: compiling cargo-audit from source is forbidden; use taiki-e/install-action"
        )
    if "cargo audit --ignore RUSTSEC-2023-0071" not in security_text:
        failures.append(".github/workflows/security.yml: weekly/manual audit command is required")
    if "continue-on-error: true" in security_text:
        failures.append(".github/workflows/security.yml: dependency audit must fail closed")
    if "github.ref" not in security_text and "concurrency:" in security_text:
        failures.append(".github/workflows/security.yml: concurrency must not collapse unrelated refs")


# ---------------------------------------------------------------------------
# Gate self-defence.
#
# Two commits switched off most of this repository's verification without a
# failing test to justify it: 536092c ("fix(deploy): atomic Caddy cutover")
# dropped 937 contract assertions from `just ci`, and 80c934a ("feat: add
# Telegram poster") deleted both ratchets outright. Neither showed up in
# review, because the thing that would have caught it was the thing being
# removed.
#
# So the gates now guard each other: removing one fails this check. Retiring a
# gate on purpose means deleting its clause here too, which is a visible line
# in the diff instead of a silent absence.
# ---------------------------------------------------------------------------
REQUIRED_GATE_SCRIPTS = [
    "scripts/source-size-ratchet.py",
    "scripts/api-sql-ratchet.py",
    "scripts/test_platform_vocabulary_v1.py",
    "scripts/test_sql_identifiers_v1.py",
]
for relative in REQUIRED_GATE_SCRIPTS:
    if not (ROOT / relative).exists():
        failures.append(f"{relative}: required gate script was deleted")

# `unittest discover` is how the ~937 source-reading assertions run. Nothing
# else invokes them, so losing this one line silently disables all of them.
DISCOVER = "unittest discover -s scripts -p 'test_*.py'"
justfile = ROOT / "justfile"
if justfile.exists():
    just_text = justfile.read_text()
    if DISCOVER not in just_text:
        failures.append("justfile: the contract-test suite is no longer invoked")
    ci_recipe = next(
        (line for line in just_text.splitlines() if line.startswith("ci:")),
        "",
    )
    for recipe in ("check", "contract-tests", "policy-checks"):
        if recipe not in ci_recipe:
            failures.append(f"justfile: `just ci` no longer runs `{recipe}`")

ci_workflow = ROOT / ".github/workflows/ci.yml"
if ci_workflow.exists():
    ci_text = ci_workflow.read_text()
    if DISCOVER not in ci_text:
        failures.append(".github/workflows/ci.yml: the contract-test suite is no longer invoked")
    for relative in REQUIRED_GATE_SCRIPTS:
        if relative not in ci_text:
            failures.append(f".github/workflows/ci.yml: no longer runs {relative}")
    # Postgres targets are enumerated from the tree. A hand-written list
    # covered 9 of 24 suites and nobody noticed the other 15 never ran.
    if "crates/*/tests/*_postgres.rs" not in ci_text:
        failures.append(
            ".github/workflows/ci.yml: Postgres suites must be discovered, not listed by hand"
        )


if failures:
    for failure in failures:
        print(f"CI_POLICY=FAIL {failure}", file=sys.stderr)
    raise SystemExit(1)
print("CI_POLICY=PASS actions=sha-pinned netlify=source-build-disabled")
