#!/usr/bin/env python3
"""Fail if deploy/runtime config regresses below the PostgreSQL 18 contract."""
from pathlib import Path
import re, sys
ROOT = Path(__file__).resolve().parents[1]
SCAN = [ROOT/'docker-compose.yml', ROOT/'compose.production.yaml', ROOT/'.github', ROOT/'ops']
allowed: set[Path] = set()
patterns = [re.compile(r'postgres\s*:\s*(?:1[0-7])(?:\D|$)', re.I), re.compile(r'postgres:(?:1[0-7])(?:[-@\s]|$)', re.I)]
violations=[]
for base in SCAN:
    files = [base] if base.is_file() else list(base.rglob('*')) if base.exists() else []
    for p in files:
        if not p.is_file() or p in allowed or p.suffix.lower() not in {'.yml','.yaml','.toml','.sh','.env','.example',''}:
            continue
        try: text=p.read_text()
        except UnicodeDecodeError: continue
        for i,line in enumerate(text.splitlines(),1):
            if any(rx.search(line) for rx in patterns):
                violations.append(f"{p.relative_to(ROOT)}:{i}: {line.strip()}")
if violations:
    print('POSTGRES_MAJOR_POLICY=FAIL minimum=18')
    print('\n'.join(violations))
    sys.exit(1)
print('POSTGRES_MAJOR_POLICY=PASS minimum=18')
