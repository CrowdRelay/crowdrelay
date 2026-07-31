# Weighted draws and event synchronization

## Weighted draws

The worker leases scheduled draws after `draw_at`, locks the draw, snapshots all eligible candidates and calculates bounded weights. Selection is without replacement using a deterministic HMAC-based exponential-race score. The run stores a seed commitment before selection and the seed reveal after completion.

Current production example:

- one global physical-album draw;
- three winners total;
- all active fans are eligible;
- referrals and concert check-ins add entries;
- guest-list draws remain event-specific and do not receive check-in entries.

The runtime kill switch defaults to off. Bootstrap is idempotent, but production uses `deploy/bootstrap.production.json`, which installers deliberately preserve. Merge reviewed changes from the example file before deployment.

## Bandsintown ingestion

`event_sources` stores provider configuration and lease state. The worker fetches outside the transaction with timeout, a 2 MiB body limit and a 500-event cap. It validates and normalizes records, then upserts by provider identity.

A first empty feed is treated as suspicious and retains existing events. Only a subsequent authoritative empty result can cancel missing provider events. Provider updates do not overwrite curated title, description, city, venue, address, timezone or public URL on manually adopted events.
