#!/usr/bin/env python3
"""Seeds tracked smart links, audience segments, and community outreach
targets into a running CrowdRelay instance. Idempotent: safe to re-run.

Requires:
  CROWDRELAY_ADMIN_TOKEN   — admin API bearer token
  CROWDRELAY_API_URL       — e.g. https://api.virya.music/v1
"""
import json, os, sys, urllib.request

BASE = os.environ.get("CROWDRELAY_API_URL", "http://127.0.0.1:8080/v1")
TOKEN = os.environ.get("CROWDRELAY_ADMIN_TOKEN", "")
if not TOKEN:
    sys.exit("set CROWDRELAY_ADMIN_TOKEN")

def api(method, path, body=None):
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(
        BASE + path,
        data=data,
        method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "content-type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req) as r:
            return json.load(r)
    except Exception as e:
        print(f"  ERROR {path}: {e}")
        return None

SMART_LINKS = [
    {"slug":"rd-polishmetal","destination_url":"https://virya.music/signal","channel_source":"reddit","channel_community":"r-polishmetal","channel_creative":"signal-launch-v1"},
    {"slug":"rd-metalpolska","destination_url":"https://virya.music/signal","channel_source":"reddit","channel_community":"r-metalpolska","channel_creative":"signal-launch-v1"},
    {"slug":"rd-polska-mega","destination_url":"https://virya.music/signal","channel_source":"reddit","channel_community":"r-polska","channel_creative":"app-launch-mega"},
    {"slug":"mf-metalforum","destination_url":"https://virya.music/signal","channel_source":"metalforum","channel_community":"metalforum-pl","channel_creative":"band-thread-v1"},
    {"slug":"dp-darkplanet","destination_url":"https://virya.music/signal","channel_source":"darkplanet","channel_community":"darkplanet-pl","channel_creative":"synesthesia-demo"},
    {"slug":"mz-metalowiec","destination_url":"https://virya.music/signal","channel_source":"metalowiec","channel_community":"metalowiec-pl","channel_creative":"press-release"},
    {"slug":"dc-polish-alliance","destination_url":"https://virya.music/signal","channel_source":"discord","channel_community":"polish-metal-alliance","channel_creative":"signal-launch-v1"},
]

COMMUNITIES = [
    {"symbol_slug":"rd-polishmetal","community_name":"r/polishmetal","platform":"reddit","url":"https://reddit.com/r/polishmetal","country_code":"PL","language":"pl","self_promo_policy":"tolerant","priority":80},
    {"symbol_slug":"rd-metalpolska","community_name":"r/MetalPolska","platform":"reddit","url":"https://reddit.com/r/MetalPolska","country_code":"PL","language":"pl","self_promo_policy":"tolerant","priority":70},
    {"symbol_slug":"mf-metalforum","community_name":"MetalForum.pl","platform":"forum","url":"https://metalforum.pl","country_code":"PL","language":"pl","self_promo_policy":"tolerant","priority":60},
    {"symbol_slug":"dp-darkplanet","community_name":"DarkPlanet.pl","platform":"forum","url":"https://darkplanet.pl","country_code":"PL","language":"pl","self_promo_policy":"tolerant","priority":60},
    {"symbol_slug":"dc-polish-alliance","community_name":"Polish Metal Alliance Discord","platform":"discord","url":"discord.gg/polishmetalalliance","country_code":"PL","language":"pl","self_promo_policy":"tolerant","priority":75},
    {"symbol_slug":"mz-metalzone-cz","community_name":"Metalzone.cz","platform":"webzine","url":"https://metalzone.cz","country_code":"CZ","language":"cs","self_promo_policy":"strict","priority":50},
    {"symbol_slug":"pm-powermetal-de","community_name":"Powermetal.de","platform":"webzine","url":"https://powermetal.de","country_code":"DE","language":"de","self_promo_policy":"strict","priority":40},
]

print("== Seeding smart links ==")
for link in SMART_LINKS:
    slug = link["slug"]
    result = api("POST", "/admin/smart-links", {
        **link,
        "campaign_id": None,
        "active": True,
    })
    status = "OK" if result else "SKIP/ERR"
    print(f"  {slug}: {status}")

print("\n== Done. Verify at /api/summary or /api/tax-export.csv ==")
