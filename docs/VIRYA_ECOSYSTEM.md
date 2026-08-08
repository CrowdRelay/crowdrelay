# VIRYA ecosystem

Four deployable products, one bounded system:

```text
virya.music ───────┐
Virya Signal ──────┼──> CrowdRelay API/worker ──> PostgreSQL / outbox / Proof of Fair
Synesthesia ───────┘
       │
       └──────────────> virya.music / Signal entry points
```

## Ownership

| Component | Owns | Must not own |
| --- | --- | --- |
| `crowdrelay` | fans, consent, audience segments, communication intents, events, tickets, inventory, draw state, Synesthesia completion ledger, audit/proofs | presentation, provider message rendering, game rendering |
| `virya` | public web, trusted Netlify edge, Stripe/web mail edge, staff UI | canonical fan/ticket/draw state |
| `virya-signal` | native fan/staff UX, encrypted local credentials/wallet | server authority, duplicated commerce or draw logic |
| `synesthesia` | album experience, local progress, sensory/rendering state | marketing consent, shipping PII, winner selection |

Cross-product integration is API/deep-link based. No product imports another product's runtime.

## Synesthesia contract

Campaign: `virya-synesthesia-album-v1`.

1. `POST /v1/public/synesthesia/runs` starts/resumes a pseudonymous run.
2. Eleven ordered room completions are recorded with the run bearer.
3. `POST .../complete` closes the run only after all rooms are present.
4. The player may submit one e-mail to `/v1/public/synesthesia/reward-claims`.
5. CrowdRelay records at most one entry per normalized e-mail and campaign.
6. A standard physical-item reward draw selects exactly **5** winners with **1 equal entry per participant**.
7. Existing CrowdRelay inventory reservations and Proof-of-Fair receipts handle stock and auditability.

The draw endpoint accepts entries only while the matching scheduled campaign window is open. It does not update `fan_consents`, enqueue marketing mail, collect shipping addresses, or add referral/check-in weight. Only selected winners enter the existing fulfillment flow later.

## Rollout order

1. **CrowdRelay** — migration `0030_synesthesia_ecosystem.sql`, API, worker.
2. Verify `/v1/health/ready`, worker health, existing mailer regression gates and OpenAPI assets.
3. **virya.music** — ecosystem links and staff campaign UI.
4. In staff Commerce create and schedule a Synesthesia campaign using a physical CD SKU with at least five available units. The server reserves the pool and locks: 5 winners, 1 unit each, one entry, no bonuses. Set the draw window to cover the public action.
5. **Virya Signal** — Synesthesia entry point. Failure must degrade to an ordinary external link only.
6. **Synesthesia** — CrowdRelay lifecycle, draw entry, HiDPI web shell, custom splash and app icons.

Do not mass-reimport n8n workflows for this rollout. Synesthesia adds no n8n/mail branch.

## Rollback

- Frontends can roll back independently; additive CrowdRelay tables/routes can remain unused.
- To stop new entries without touching mail/ticket flows, remove/disable the Synesthesia reward entry UI first.
- Cancel a scheduled Synesthesia reward campaign through the existing staff commerce flow to release its inventory reservation.
- Do not drop migration `0030` while runs, entries, or a draw reference it. Leaving additive tables dormant is the safe rollback.

## Release gates

```text
crowdrelay:   Synesthesia contract tests + commerce/draw tests + OpenAPI validator
virya:        64 node contract tests + source audits + production build in normal CI
virya-signal: static IPC/i18n/layout/performance contracts + Cargo/Tauri CI
synesthesia:  Python renderer/audio/memory/contracts + Godot import/runtime/export in CI
```

Secrets stay server/native-only. Shipping PII never belongs in Synesthesia. Public analytics/marketing must remain optional and outside reward eligibility.
