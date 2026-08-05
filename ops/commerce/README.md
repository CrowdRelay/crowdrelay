# Virya commerce and reward campaigns

This directory contains the initial Virya product catalog and a deliberately
blank physical-stock worksheet. The new inventory path is staged behind three
CrowdRelay feature flags and one Virya site environment switch.

## Safety order

1. Apply database migration `0027_merch_inventory_reward_campaigns.sql`.
2. Deploy the CrowdRelay API and worker with every new flag still disabled.
3. Enable only `merch_inventory_writes_enabled` while the Virya site variable
   `CROWDRELAY_MERCH_INVENTORY_WRITES_ENABLED` remains unset/false.
4. Import `virya-catalog.seed.json` through `POST /v1/admin/merch/catalog`.
5. Count every physical SKU and fill `virya-initial-stock.template.csv`.
6. Record the initial quantities as idempotent `initial` ledger adjustments.
7. Enable `merch_inventory_enabled` and verify public read-only availability.
8. Deploy Virya and Virya Signal, then verify graceful timeout/error states.
9. Enable the Virya site write switch for one controlled Stripe canary order.
10. Enable `reward_campaigns_enabled` only after the order canary and rollback
    drill have passed.

Do not down-migrate during an incident. Disable new writes first and let signed
Stripe webhook reconciliation continue through the internal commit/release
endpoints.

## Feature flags

- `merch_inventory_enabled`: public read model is available.
- `merch_inventory_writes_enabled`: catalog, manual stock, order reservations,
  campaign creation and promotional issue mutations are allowed.
- `reward_campaigns_enabled`: draft campaigns may be scheduled and the worker
  may pick them up through the existing draw mechanism.

All three default to `false`.

## Stock import rule

Never infer stock from the old static web values. The CSV intentionally has
blank quantities. Count the physical stock first and use stable idempotency keys
such as `initial-stock:VIRYA-CD-ECHOES:v1` for each adjustment.
