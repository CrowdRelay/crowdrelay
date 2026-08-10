#!/usr/bin/env python3
"""Stamp the canonical JS client with the OpenAPI fingerprint and sync mirrors.

Run with --check in CI. With --write it updates the canonical client header,
the in-repo Virya integration mirror, and (when present in an ecosystem checkout)
../virya/src/lib/crowdrelay-client.ts.
"""
from pathlib import Path
import argparse, hashlib, re, sys
ROOT=Path(__file__).resolve().parents[1]
OPENAPI=ROOT/'openapi/openapi.yaml'
CANON=ROOT/'packages/crowdrelay-js/src/index.ts'
MIRRORS=[ROOT/'integration/virya/src/lib/crowdrelay-client.ts']
external=ROOT.parent/'virya/src/lib/crowdrelay-client.ts'
if external.exists(): MIRRORS.append(external)
PREFIX='// @generated-contract openapi-sha256:'

def digest(p: Path) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()

def stripped(text: str) -> str:
    lines=text.replace('\r\n','\n').splitlines()
    if lines and lines[0].startswith(PREFIX): lines=lines[1:]
    return '\n'.join(lines).lstrip('\n')+'\n'

parser=argparse.ArgumentParser()
g=parser.add_mutually_exclusive_group(required=True)
g.add_argument('--write',action='store_true'); g.add_argument('--check',action='store_true')
a=parser.parse_args()
sha=digest(OPENAPI)
body=stripped(CANON.read_text())
expected=f'{PREFIX}{sha}\n{body}'
if a.write:
    CANON.write_text(expected)
    for m in MIRRORS:
        m.parent.mkdir(parents=True, exist_ok=True)
        m.write_text(expected)
    print(f'CLIENT_CONTRACT_SYNC=PASS mode=write openapi_sha256={sha} mirrors={len(MIRRORS)}')
    raise SystemExit(0)
failed=[]
if CANON.read_text().replace('\r\n','\n') != expected:
    failed.append(str(CANON.relative_to(ROOT)))
for m in MIRRORS:
    if m.read_text().replace('\r\n','\n') != expected:
        failed.append(str(m.relative_to(ROOT.parent) if ROOT.parent in m.parents else m))
if failed:
    print('CLIENT_CONTRACT_SYNC=FAIL stale=' + ','.join(failed))
    print('Run: python3 scripts/sync-client-contract.py --write')
    raise SystemExit(1)
print(f'CLIENT_CONTRACT_SYNC=PASS mode=check openapi_sha256={sha} mirrors={len(MIRRORS)}')
