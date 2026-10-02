# Deploy checklist — main since `c78365b3` (prod, schema 405)

Written 2026-10-03. Prod `/v1/meta` read `gitSha c78365b3`, `schemaVersion 405`;
`max(version) from _sqlx_migrations` was 405. **Everything below is merged and
unmeasured.** FAN_100_PLAN_V2 P0.5 (one real campaign with full denominators)
cannot start until this ships. Commands are copy-paste; nothing here deploys.

## 1. Migrations 0406–0412 (all additive or rename-only)

| # | What it does | Prod data it touches |
| --- | --- | --- |
| 0406 | widens `person_identities` kinds (adds `platform_user_id`); replaces unique `fan_prospects` index with `(workspace, person, platform)`; **renames** `scout_*` tables to `legacy_0404_*` (no drop) | prod holds 10 prospects / 10 persons / 13 observations, one person each → the new unique index cannot collide; `scout_prospects` is empty |
| 0407 | widens the `fan_prospect_observations.observation_kind` CHECK (adds `asked_to_join_or_follow`) | widening only, validates existing rows |
| 0408 | new `fan_prospect_touches` | none |
| 0409 | `fan_prospect_agent_outcome` | none |
| 0410 | new `latarnik_roles` (person-keyed) | none; no tenant enrolled |
| 0411 | new `fan_advocacy_opportunities` | none |
| 0412 | adds `UNIQUE (workspace_id, id)` on `latarnik_roles`; new `latarnik_missions` | none |

Rollback of schema is not provided (forward-only migrator); every change above is
safe to leave in place under the previous binary because nothing is dropped.

## 2. Deploy order (backward-compatible)

1. **crowdrelay** (api + worker): adds `/v1/me/latarnik*` and the control-plane
   growth routes; old Signal clients never call them.
2. **virya** (site/Signal web): `virya#54` renders the invitation and mission cards
   from those routes. Deploy *after* step 1, or the card calls a 404.
3. n8n workflow `VOSTEAMOPS00001`: already edited live on 2026-10-02
   (content artifacts fail fast as `artifact_surface_unavailable`; original in
   `~/dev/crowdrelay-VOSTEAMOPS00001.backup-2026-10-02.json`). Commit the JSON into
   the release manifest so a deploy does not overwrite it.

## 3. Behaviour that changes on deploy (so a drop is not mistaken for a regression)

- **Clicks fall.** Link previews, crawlers, HEAD and prefetches are no longer
  recorded (#471). Historical `click_events` are not cleaned; compare only
  post-deploy windows. Counter: `crowdrelay_tracked_link_fetches_not_clicked_total{reason}`.
- **Workers now run by default:** `prospect_sweep` (own-comment prospects, hourly),
  `latarnik_sweep` (candidates + missions, hourly). Neither contacts anyone.
- **The reply senders can halt** on a `scout.*` breach (`scout_lane::halted`); an
  unreadable lane is a halt.
- **Shorts are never promoted** and unclassified YouTube watcher rows are held until
  the next sweep classifies them (#487) — expect a short gap in video promotion.
- **First referral ask** now needs typed advocacy readiness (#498), not signup age.

## 4. Owner-only switches (not touched by code)

`social_auto_post` (currently false; `social_autopost_platforms=telegram`),
`CROWDRELAY_CONTACT_RESEARCH_SWEEP`, Reddit write posture. YouTube OAuth is
connected (`youtube_account`, 2026-10-02) but lacks the analytics scope — re-run
`/v1/public/connections/youtube_account/authorize` with analytics ticked for
traffic-source split. The Meta page token in the worker env was verified to hold
`pages_manage_posts` + `instagram_content_publish`.

## 5. Proofs to run after deploy (null is a result; invented success is not)

```sh
# a. schema + build
curl -s https://signal-api.virya.music/v1/meta | jq '{gitSha, schemaVersion}'   # expect 412+

# b. the lane ledger — where each delivery lane stops (#508)
#    GET /v1/control-plane/growth/lanes?days=14   (control-plane bearer)
#    Reddit read `held_for_person` on 2026-10-02 (15 held); telegram `delivering`.

# c. the organic funnel for the live placement (existing)
#    GET /v1/control-plane/ops/organic-funnel?days=30

# d. person layer + advocacy
#    GET /v1/control-plane/growth/prospect-funnel     # counts only
#    GET /v1/control-plane/growth/prospect-actions    # the typed next-best actions
#    GET /v1/control-plane/growth/latarnik-roles
#    GET /v1/control-plane/growth/viral-coefficient   # K withheld (null) below 4 in a cohort — expected at ~20 fans

# e. B0 — one real signup through a /watch link stores an anonymous_visitor_id
psql -c "select count(*) filter (where anonymous_visitor_id is not null), count(*) from fan_acquisition_events where created_at > now() - interval '1 day'"
```

**Expected zeros at first read** (they are the baseline, not a failure): K `null`,
`latarnik_roles` empty until the first candidate is detected (needs a retained fan
who was in the room or purchased), prospect actions limited to the ~10 current
commenters, clicks lower than the prior week for the bot-filter reason above.

## 6. The first real campaign (V2 P0.5)

Pick one upcoming show (10-17 Gorzów) or the newest release with a concrete promise,
mint its tracked placements (join kit + show kit are live), and record the ladder
requested → eligible → approved → dispatched → delivered → visited → clicked → joined
→ activated → retained → converted → referred from the ledgers above. A stage with
zero is allowed; the first stage that reads zero or `held_for_person` is the next
bottleneck.
