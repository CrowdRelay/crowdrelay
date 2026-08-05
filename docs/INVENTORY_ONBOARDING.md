# Inventory onboarding and READY activation

Migration `0028_inventory_onboarding.sql` is additive and must run after the corrected migration 0027. It creates the onboarding state and exact stocktake ledger, then idempotently seeds the canonical Virya catalog for every workspace existing at migration time.

The seed contains six products and 22 active SKUs:

- album CD;
- four T-shirt designs in S, M, L, XL and XXL;
- one bag variant.

The seed never invents stock. Until staff explicitly records every active SKU, the public catalog, stock reservations and reward campaigns remain disabled.

## Staff flow

Open `/staff/commerce/`:

1. The panel loads the seeded products and all active variants.
2. Enter the exact physical count for every SKU. Explicit `0` is valid and is different from an uncounted SKU.
3. Click **ZAPISZ DOKŁADNY STAN**. This records one idempotent stocktake and ledger adjustments.
4. Review the preflight. READY remains disabled when:
   - the catalog is empty;
   - any active SKU is uncounted;
   - active reservations exceed physical stock.
5. Click **MAGAZYN GOTOWY — READY**.

The READY endpoint locks the activation row and active variants, reruns the preflight and then, in one database transaction:

- marks inventory as ready;
- enables public inventory reads;
- enables order reservations and Stripe stock writes;
- enables reward campaigns.

No Netlify environment edit or redeploy is required. The checkout checks the authoritative CrowdRelay activation state on each new purchase and starts using inventory reservations immediately after READY.

## State shown after activation

For every variant the staff panel separates:

- physical on-hand quantity;
- order reservations;
- campaign reservations;
- operational reservations;
- available-to-sell quantity;
- total and 30-day sales;
- promotional issues;
- active campaign count.

Subsequent stock corrections use the existing inventory ledger. A partially inconsistent set of feature flags can be repaired with the same READY button; the physical counts are not rewritten.

## Safety and rollback

Before READY, commerce behavior remains compatible with the previous deployment. To pause new commerce writes after activation, use the existing feature flags. Do not delete ledger rows or stocktake records. Migration rollback is not required for an operational pause.

This onboarding does not modify ticketing tables, fan lifecycle, mail checkpoints, outbox payloads or n8n workflows.
