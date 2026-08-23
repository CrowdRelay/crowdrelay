# Virya commerce inventory onboarding

The canonical catalog lives in `virya-catalog.seed.json`; migration `0028_inventory_onboarding.sql` contains the same idempotent seed so the catalog exists before the updated API starts. The CSV is a printable counting aid only. The authoritative stocktake is entered in `/staff/commerce/`.

## Safe activation

1. Apply the commerce migrations through the normal `crowdrelay-worker setup` flow.
2. Deploy API and worker with commerce feature flags disabled.
3. Enter an explicit physical quantity for every SKU, including zeros.
4. Save the exact stocktake and review physical stock, reservations and available quantity.
5. Use the READY flow or `ops/commerce/activate-inventory.sh` to perform the locked preflight and enable commerce atomically.

READY enables the commerce inventory, inventory writes and reward-campaign flags together. The public catalog reads activation state dynamically; no direct flag edits or redeploy are required.

Do not infer quantities from the old static page. During an incident, pause the feature flags and preserve the inventory ledger, stocktakes and active Stripe reservation reconciliation.
