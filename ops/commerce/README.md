# Virya commerce inventory onboarding

The canonical catalog lives in `virya-catalog.seed.json`; migration `0028_inventory_onboarding.sql` contains the same idempotent seed so the catalog exists before the updated API starts. The CSV remains a printable counting aid only. The authoritative stocktake is entered in `/staff/commerce/`.

## Safe order

1. Apply the corrected migration 0027 and migration 0028 through the normal `crowdrelay-worker setup` command.
2. Deploy API and worker. All three commerce feature flags remain disabled.
3. Open `/staff/commerce/`. Verify the six products and 22 active SKUs.
4. Enter the exact physical quantity for every SKU, including explicit zeros.
5. Click **ZAPISZ DOKŁADNY STAN**.
6. Review the split between physical stock, order reservations, campaign reservations and available quantity.
7. Click **MAGAZYN GOTOWY — READY**.

READY performs a fresh locked preflight and atomically enables:

- `merch_inventory_enabled`;
- `merch_inventory_writes_enabled`;
- `reward_campaigns_enabled`.

The Virya checkout reads the activation state dynamically. No Netlify environment change or redeploy is needed after clicking READY.

Do not infer quantities from the old static page. The CSV intentionally has blank values. Do not down-migrate during an incident: pause the feature flags and preserve the ledger, stocktakes and active Stripe reservation reconciliation.

## Safe CLI activation

After all 22 SKUs have an explicit stocktake, activate and verify the public catalog without touching feature flags directly:

```bash
sudo ./ops/commerce/activate-inventory.sh
```

The script uses the staff key already present inside `crowdrelay-api`, refuses activation while the locked server preflight reports blockers, calls the canonical READY endpoint, and verifies `/v1/public/merch/catalog` before succeeding.
