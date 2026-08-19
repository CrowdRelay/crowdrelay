#!/usr/bin/env python3
"""Idempotently add VIRYA affiliate smart links to a private bootstrap JSON.
Does not resolve or print secrets; only edits the two server-owned redirect URLs.
"""
from __future__ import annotations
import argparse, json
from pathlib import Path
LINKS={
 'thomann-qc-signal':'https://www.thomann.pl/neural_dsp_quad_cortex.htm?offid=1&affid=4979&subid=virya_music&subid2=gear',
 'thomann-shop-signal':'https://www.thomann.pl/?offid=1&affid=4979&subid=virya_music&subid2=shop',
}
ap=argparse.ArgumentParser(); ap.add_argument('bootstrap',type=Path); ap.add_argument('--check',action='store_true'); a=ap.parse_args()
d=json.loads(a.bootstrap.read_text()); rows=d.setdefault('smart_links',[])
by={row.get('slug'):row for row in rows if isinstance(row,dict)}
changed=False
for slug,url in LINKS.items():
    wanted={'slug':slug,'destination_url':url,'active':True}
    if by.get(slug)!=wanted:
        if slug in by: by[slug].update(wanted)
        else: rows.append(wanted)
        changed=True
if a.check:
    if changed: raise SystemExit('AFFILIATE_SMART_LINKS=DRIFT')
    print('AFFILIATE_SMART_LINKS=PASS'); raise SystemExit(0)
a.bootstrap.write_text(json.dumps(d,indent=2)+"\n")
print(f'AFFILIATE_SMART_LINKS={"UPDATED" if changed else "UNCHANGED"}')
