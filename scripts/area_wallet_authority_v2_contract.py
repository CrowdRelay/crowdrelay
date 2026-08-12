#!/usr/bin/env python3
"""Static contract: AREA wallet economics are canonical in CrowdRelay/Postgres."""
from pathlib import Path
import sys
ROOT = Path(__file__).resolve().parents[1]
ECO = ROOT.parent
VIRYA = ECO/'virya'
checks=[]

def must(path, needles):
    text=path.read_text()
    for n in needles:
        checks.append((n in text, f"{path.relative_to(ECO)} contains {n!r}"))

def must_not(path, needles):
    text=path.read_text()
    for n in needles:
        checks.append((n not in text, f"{path.relative_to(ECO)} excludes {n!r}"))

must(ROOT/'migrations/0036_area_wallet_postgres_authority.sql', [
    'CREATE TABLE IF NOT EXISTS area_credit_ledger',
    'CREATE TABLE IF NOT EXISTS area_reward_vouchers',
    'CREATE TABLE IF NOT EXISTS area_ticket_rewards',
    'CREATE TABLE IF NOT EXISTS area_legacy_wallet_imports',
])
must(ROOT/'crates/crowdrelay-api/src/area/claims.rs', ['insert_credit_delta'])
must(ROOT/'crates/crowdrelay-api/src/area/ticket_rewards.rs', ['AREA ticket reward idempotency lookup failed', 'existing.fan_email != email', 'valid_small_text(&payload.reservation_id, 128)'])
must(ROOT/'crates/crowdrelay-api/src/area/legacy_wallet.rs', [
    'HashSet::with_capacity',
    'ON CONFLICT (workspace_id, request_id) DO NOTHING',
    'LEGACY_IMPORT_CONFLICT',
    'source_voucher_count',
    'source_ticket_reward_count',
])
must_not(ROOT/'crates/crowdrelay-api/src/area/legacy_wallet.rs', ['expect("validated above")', 'ON CONFLICT DO NOTHING'])
must(ROOT/'crates/crowdrelay-api/src/routing.rs', [
    '/v1/internal/area/players/{player_id}/wallet/import',
    '/v1/internal/area/players/{player_id}/vouchers',
    '/v1/internal/area/players/{player_id}/ticket-rewards/reserve',
    '/v1/internal/area/rewards/redeem',
])
if VIRYA.exists():
    # Current production migration stage: CrowdRelay owns claim authority;
    # Virya mirrors canonical claims into the legacy reward ledger exactly once
    # until voucher/checkout spending is cut over in a dedicated release.
    must(VIRYA/'src/server/crowdrelayArea.ts', ['getAreaBackendWallet', 'importLegacyAreaClaims', 'me/area/claim'])
    must(VIRYA/'src/server/areaLegacySync.ts', ['syncBackendClaimsToLegacyWallet', 'wallet.tokenBalance + additions.length'])
    must(VIRYA/'src/pages/api/area/wallet.ts', ['importLegacyAreaClaims', 'missingLegacyClaims', 'syncBackendClaimsToLegacyWallet'])

failed=[msg for ok,msg in checks if not ok]
if failed:
    print('AREA_WALLET_AUTHORITY_V2=FAIL')
    print('\n'.join(' - '+x for x in failed))
    sys.exit(1)
print(f'AREA_WALLET_AUTHORITY_V2=PASS checks={len(checks)} claims=postgres rewards=staged-legacy-ledger')
