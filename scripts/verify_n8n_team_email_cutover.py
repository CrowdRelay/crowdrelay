#!/usr/bin/env python3
import argparse,json,re
from pathlib import Path
EVENT="viryaos.team.assignment_email_requested"; CANONICAL="VOSTEAMEMAIL001"; HEX=re.compile(r"^[0-9a-f]{64}$")
p=argparse.ArgumentParser(); p.add_argument("inventory",type=Path); a=p.parse_args(); data=json.loads(a.inventory.read_text()); ws=data.get("workflows")
if not isinstance(ws,list): raise SystemExit("TEAM_EMAIL_CUTOVER=FAIL workflows-missing")
active=[w for w in ws if isinstance(w,dict) and w.get("active") is True and EVENT in (w.get("events") if isinstance(w.get("events"),list) else [])]
canonical=[w for w in active if w.get("id")==CANONICAL]
if len(canonical)!=1: raise SystemExit(f"TEAM_EMAIL_CUTOVER=FAIL canonical-active={len(canonical)}")
if len(active)!=1: raise SystemExit("TEAM_EMAIL_CUTOVER=FAIL duplicate-active-handlers="+','.join(str(w.get('id')) for w in active))
row=canonical[0]; sha=row.get("workflowSha256"); att=data.get("attestedWorkflowSha256")
if not isinstance(sha,str) or not HEX.fullmatch(sha): raise SystemExit("TEAM_EMAIL_CUTOVER=FAIL workflow-sha-missing")
if not isinstance(att,str) or att!=sha: raise SystemExit("TEAM_EMAIL_CUTOVER=FAIL attestation-sha-mismatch")
if row.get("saveExecutionProgress") is True: raise SystemExit("TEAM_EMAIL_CUTOVER=FAIL execution-persistence-enabled")
print(f"TEAM_EMAIL_CUTOVER=PASS workflow={CANONICAL} event={EVENT} sha={sha}")
