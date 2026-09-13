#!/usr/bin/env python3
"""Builds the CrowdRelay system reference as a dark-mode PDF.

Two stages. This script reads the repository and emits one self-contained HTML
file; `render_reference.mjs` then drives the Chromium that `crowdrelay-agents`
already installs for Playwright and prints it to PDF. Nothing else is installed
on this machine that can render CSS to PDF — no pandoc, no wkhtmltopdf, no
weasyprint, no Chrome — so the Reddit automation's browser is the renderer.

Every number in the output is measured from the tree at build time rather than
typed into prose. A reference that quotes a stale count is worse than one that
omits it, and this document exists because the system outgrew somebody's memory
of it.

Usage:
    python3 ops/docs/build_reference.py            # writes the HTML
    node ops/docs/render_reference.mjs            # writes the PDF
    just reference                                 # both
"""
from __future__ import annotations

import html
import json
import re
import subprocess
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT_HTML = ROOT / "ops/docs/build/system-reference.html"

ROUTE_FILES = [
    "routing.rs",
    "control_plane.rs",
    "ops_routes.rs",
    "area_admin.rs",
    "synesthesia.rs",
    "portfolio.rs",
    "audience_graph.rs",
    "community_intelligence_routes.rs",
    "routing/growth.rs",
]

CRATES = [
    ("crowdrelay-api", "HTTP boundary: auth, validation, response contracts"),
    ("crowdrelay-infra", "SQL, providers, config, observability"),
    ("crowdrelay-worker", "Background loops: outbox, executors, watchdog"),
    ("crowdrelay-domain", "Pure policy: pricing, admission, autopilot rules"),
    ("crowdrelay-application", "Use cases and ports. Holds zero SQL call sites"),
    ("crowdrelay-brain", "World model, strategy, causal model, EFE, validation"),
]


# ── measurement ────────────────────────────────────────────────────────────────


def sh(command: str) -> str:
    return subprocess.run(
        command, shell=True, cwd=ROOT, capture_output=True, text=True
    ).stdout.strip()


def crate_sizes() -> list[tuple[str, str, int, int]]:
    rows = []
    for name, purpose in CRATES:
        src = ROOT / "crates" / name / "src"
        files = list(src.rglob("*.rs"))
        lines = sum(len(p.read_text(errors="replace").splitlines()) for p in files)
        rows.append((name, purpose, len(files), lines))
    return rows


def route_counts() -> list[tuple[str, int]]:
    rows = []
    for name in ROUTE_FILES:
        path = ROOT / "crates/crowdrelay-api/src" / name
        count = path.read_text().count(".route(") if path.exists() else 0
        rows.append((name, count))
    return sorted(rows, key=lambda row: -row[1])


def prefix_counts() -> list[tuple[str, int]]:
    joined = ""
    for name in ROUTE_FILES:
        path = ROOT / "crates/crowdrelay-api/src" / name
        if path.exists():
            joined += path.read_text()
    found: dict[str, int] = {}
    for match in re.finditer(r'"(/v1/[a-z-]+)', joined):
        found[match.group(1)] = found.get(match.group(1), 0) + 1
    return sorted(found.items(), key=lambda row: -row[1])


def autopilot_contexts() -> list[str]:
    """The 22 contexts, read from the CHECK constraint that defines them."""
    values: set[str] = set()
    for path in sorted((ROOT / "migrations").glob("*.sql")):
        text = path.read_text()
        for match in re.finditer(
            r"viryaos_autopilot_policies_context_check(.{0,600}?)\)\)", text, re.DOTALL
        ):
            values.update(re.findall(r"'([a-z_]+)'", match.group(1)))
    return sorted(values)


def worker_modules() -> list[str]:
    lib = (ROOT / "crates/crowdrelay-worker/src/lib.rs").read_text()
    return sorted(set(re.findall(r"^\s*(?:pub )?mod ([a-z_]+);", lib, re.M)))


def brain_modules() -> list[str]:
    lib = (ROOT / "crates/crowdrelay-brain/src/lib.rs").read_text()
    return sorted(set(re.findall(r"^\s*pub mod ([a-z_]+);", lib, re.M)))


def operations_modules() -> list[str]:
    root = ROOT / "crates/crowdrelay-infra/src/autopilot/operations"
    names = {p.stem for p in root.glob("*.rs")} | {
        p.name for p in root.iterdir() if p.is_dir()
    }
    return sorted(names)


def counts() -> dict[str, int]:
    migrations = sorted((ROOT / "migrations").glob("*.sql"))
    created = sum(
        len(re.findall(r"^CREATE TABLE", p.read_text(), re.M)) for p in migrations
    )
    return {
        "migrations": len(migrations),
        "tables": created,
        "routes": sum(count for _, count in route_counts()),
        "gates": len(list((ROOT / "scripts").glob("test_*.py"))),
        "policy_scripts": len(list((ROOT / "scripts").glob("*.py")))
        + len(list((ROOT / "scripts").glob("*.sh"))),
        "openapi_paths": len(
            re.findall(r"^  /", (ROOT / "openapi/openapi.yaml").read_text(), re.M)
        ),
    }


def git_facts() -> dict[str, str]:
    return {
        "sha": sh("git rev-parse --short HEAD"),
        "subject": sh("git log -1 --pretty=%s"),
        "built": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
    }


def production_snapshot() -> dict[str, str] | None:
    """Live gauges, if the deployment answers. Omitted rather than guessed."""
    raw = sh(
        "curl -sS --max-time 20 https://signal-api.virya.music/metrics 2>/dev/null"
    )
    if not raw:
        return None
    wanted = {}
    for line in raw.splitlines():
        if line.startswith("#") or " " not in line:
            continue
        name, _, value = line.partition(" ")
        wanted[name] = value
    return wanted or None


# ── rendering ──────────────────────────────────────────────────────────────────

CSS = """
:root{
  --bg:#0d1117; --panel:#151b23; --panel-2:#1b232d; --edge:#2a3441;
  --ink:#e6edf3; --ink-dim:#9aa8b8; --ink-faint:#6e7d8f;
  --accent:#58a6ff; --good:#3fb950; --warn:#d29922; --bad:#f85149;
  --mono:"SF Mono","JetBrains Mono",Menlo,monospace;
  --sans:"Inter","SF Pro Text",-apple-system,system-ui,sans-serif;
}
/* Zero margins so the background reaches the paper edge. Chromium never paints
   the @page margin box, so any margin here prints white around a dark page.
   The text inset comes from body padding instead, and the footer template
   paints the one strip that is left. */
@page{ size:A4; margin:0; }
*{box-sizing:border-box}
html{background:var(--bg);-webkit-print-color-adjust:exact;print-color-adjust:exact}
/* Padding on the body alone. Setting it on both doubled the inset and left
   Chromium computing break positions against a box the content never used. */
body{background:var(--bg);color:var(--ink);margin:0;padding:15mm 14mm 4mm;
  font-family:var(--sans);font-size:9.6pt;line-height:1.55;
  orphans:3;widows:3;
  -webkit-print-color-adjust:exact;print-color-adjust:exact}
h1,h2,h3,h4{line-height:1.25;margin:0 0 .35em}
h1{font-size:26pt;font-weight:700;letter-spacing:-.5px}
h2{font-size:15pt;font-weight:650;margin-top:1.5em;padding-bottom:.3em;
  border-bottom:1px solid var(--edge);color:#fff;break-after:avoid}
h3{font-size:11.5pt;font-weight:650;margin-top:1.3em;color:#fff;break-after:avoid}
h4{font-size:10pt;font-weight:600;margin-top:1em;color:var(--ink);break-after:avoid}
p{margin:0 0 .7em}
a{color:var(--accent);text-decoration:none}
code,kbd{font-family:var(--mono);font-size:.88em;background:var(--panel-2);
  border:1px solid var(--edge);border-radius:3px;padding:.05em .35em;color:#c9d7e6}
pre{font-family:var(--mono);font-size:8.4pt;line-height:1.5;background:var(--panel);
  border:1px solid var(--edge);border-left:2px solid var(--accent);border-radius:5px;
  padding:.7em .9em;overflow:hidden;white-space:pre-wrap;color:#c9d7e6;
  margin:0 0 .8em;break-inside:avoid}
pre code{background:none;border:none;padding:0;font-size:inherit}
table{width:100%;border-collapse:collapse;margin:0 0 .9em;font-size:8.8pt;
  break-inside:avoid}
th,td{text-align:left;padding:.38em .55em;border-bottom:1px solid var(--edge);
  vertical-align:top}
th{color:var(--ink-dim);font-weight:600;font-size:7.8pt;text-transform:uppercase;
  letter-spacing:.06em;border-bottom:1px solid var(--edge);background:var(--panel)}
td code{font-size:.9em}
tr:last-child td{border-bottom:none}
.num{font-family:var(--mono);text-align:right;white-space:nowrap}
ul,ol{margin:0 0 .8em;padding-left:1.2em}
li{margin-bottom:.28em}
.cover{height:255mm;display:flex;flex-direction:column;justify-content:center;
  break-after:page}
.cover .kicker{font-family:var(--mono);font-size:8.5pt;color:var(--accent);
  letter-spacing:.22em;text-transform:uppercase;margin-bottom:1.6em}
.cover h1{font-size:40pt;margin-bottom:.15em}
.cover .sub{font-size:13pt;color:var(--ink-dim);font-weight:400;max-width:78%;
  line-height:1.45}
.cover .meta{margin-top:auto;font-family:var(--mono);font-size:8.5pt;
  color:var(--ink-faint);border-top:1px solid var(--edge);padding-top:1em}
.cover .meta b{color:var(--ink-dim);font-weight:500}
.panel{background:var(--panel);border:1px solid var(--edge);border-radius:6px;
  padding:.8em 1em;margin:0 0 .9em;break-inside:avoid}
.panel.note{border-left:2px solid var(--accent)}
.panel.warn{border-left:2px solid var(--warn)}
.panel.bad{border-left:2px solid var(--bad)}
.panel h4{margin-top:0}
.panel p:last-child,.panel ul:last-child{margin-bottom:0}
.grid{display:grid;grid-template-columns:repeat(4,1fr);gap:.55em;margin:0 0 1em}
.stat{background:var(--panel);border:1px solid var(--edge);border-radius:6px;
  padding:.6em .7em}
.stat .v{font-family:var(--mono);font-size:15pt;font-weight:600;color:#fff;
  line-height:1.1}
.stat .k{font-size:7.4pt;color:var(--ink-dim);text-transform:uppercase;
  letter-spacing:.06em;margin-top:.25em}
.chips{display:flex;flex-wrap:wrap;gap:.3em;margin:0 0 .9em}
.chip{font-family:var(--mono);font-size:8pt;background:var(--panel-2);
  border:1px solid var(--edge);border-radius:10px;padding:.15em .6em;
  color:#c9d7e6}
.on{color:var(--good)} .off{color:var(--bad)} .mid{color:var(--warn)}
.dim{color:var(--ink-dim)}
.flow{font-family:var(--mono);font-size:8.6pt;white-space:pre-wrap;
  background:var(--panel);
  border:1px solid var(--edge);border-radius:5px;padding:.8em 1em;margin:0 0 .9em;
  color:#c9d7e6;break-inside:avoid;line-height:1.9}
.toc{column-count:2;column-gap:2em}
.toc ol{padding-left:1.3em;margin:0}
.toc li{margin-bottom:.3em;font-size:9pt}
.pagebreak{break-before:page}
footer{margin-top:2em;padding-top:.8em;border-top:1px solid var(--edge);
  font-size:8pt;color:var(--ink-faint);font-family:var(--mono)}
"""


def esc(text: str) -> str:
    return html.escape(str(text))


def table(headers: list[str], rows: list[list[str]], numeric: set[int] | None = None) -> str:
    numeric = numeric or set()
    out = ["<table><thead><tr>"]
    for index, header in enumerate(headers):
        cls = ' class="num"' if index in numeric else ""
        out.append(f"<th{cls}>{esc(header)}</th>")
    out.append("</tr></thead><tbody>")
    for row in rows:
        out.append("<tr>")
        for index, cell in enumerate(row):
            cls = ' class="num"' if index in numeric else ""
            out.append(f"<td{cls}>{cell}</td>")
        out.append("</tr>")
    out.append("</tbody></table>")
    return "".join(out)


def chips(items: list[str]) -> str:
    return (
        '<div class="chips">'
        + "".join(f'<span class="chip">{esc(i)}</span>' for i in items)
        + "</div>"
    )


def stats(pairs: list[tuple[str, str]]) -> str:
    cells = "".join(
        f'<div class="stat"><div class="v">{esc(v)}</div>'
        f'<div class="k">{esc(k)}</div></div>'
        for k, v in pairs
    )
    return f'<div class="grid">{cells}</div>'


def main() -> int:
    from reference_content import build_document  # local module, same directory

    facts = {
        "git": git_facts(),
        "counts": counts(),
        "crates": crate_sizes(),
        "routes": route_counts(),
        "prefixes": prefix_counts(),
        "contexts": autopilot_contexts(),
        "worker": worker_modules(),
        "brain": brain_modules(),
        "operations": operations_modules(),
        "production": production_snapshot(),
    }
    body = build_document(
        facts, helpers={"esc": esc, "table": table, "chips": chips, "stats": stats}
    )
    OUT_HTML.parent.mkdir(parents=True, exist_ok=True)
    OUT_HTML.write_text(
        "<!doctype html><html><head><meta charset='utf-8'>"
        f"<title>CrowdRelay System Reference {esc(facts['git']['sha'])}</title>"
        f"<style>{CSS}</style></head><body>{body}</body></html>"
    )
    print(f"REFERENCE_HTML={OUT_HTML.relative_to(ROOT)}")
    print(f"  measured: {json.dumps(facts['counts'])}")
    return 0


if __name__ == "__main__":
    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    raise SystemExit(main())
