# Data model

Every business row is scoped by `workspace_id`.

## Main aggregates

- acquisition: campaigns, smart links, clicks, fans, sessions and consent;
- referrals: referral codes, attributions, deterministic reward rules and grants;
- events: published events, provider sources, fan interests and reminders;
- admission: pools, passes, claims and atomic redemption;
- draws: definitions, runs, candidate snapshots and winners;
- delivery: outbox events, webhook endpoints and attempts;
- concert QR: `concert_qr_campaigns` and `concert_checkins`.

## Concert QR invariants

A campaign belongs to exactly one event. The composite foreign key from check-ins to `(workspace_id, campaign_id, event_id)` prevents cross-event token use. A unique constraint on `(workspace_id, event_id, fan_id)` enforces one check-in per fan and concert even when several QR campaigns exist.

## Draw weights

A candidate receives `base_entries`, referral entries and optional concert-check-in entries. The total never exceeds `max_entries`. Referrals are applied first; check-ins fill only the remaining cap. Candidate rows persist qualified referrals, check-in counts, check-in contribution, final weight and selection score for auditability.
