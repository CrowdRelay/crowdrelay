# Deploy checklist — main since `65acf06c` (prod, schema 413)

Written 2026-10-03 night. Prod `/v1/meta` reads `gitSha 65acf06c`, `schemaVersion 413`;
`origin/main` is 52 commits ahead. Commands are copy-paste; **nothing here deploys.**
Items marked *(mine)* were written and tested in the Claude/Pi session; the rest are
summarised from commit titles and were not re-reviewed line by line.

## 1. Migration 0414 — one additive table

`0414_fan_prospect_identity_exclusions.sql` creates `fan_prospect_identity_exclusions`
(owner-declared identities FAN SCOUT must never treat as prospects). Nothing existing is
altered, renamed or backfilled. **No binary rollback after migrating:** the old binary
fails closed on `migration 414 was previously applied but is missing`. Take a `pg_dump`
(see `ops/backup`) immediately before; the dump is the rollback.

## 2. Order

1. **crowdrelay** (api + worker).
2. **virya** after it: #55 email-first watch capture, #56 notification ask at the top of
   My Signal *(mine)*, #57 editorial refresh. #55 and #57 are the parallel session's and
   were not reviewed here.

## 3. Behaviour that changes on deploy (so a drop is not mistaken for a regression)

- **The brain predicts far fewer fans per action** *(mine, #521)*: the fan prior is seeded
  from the tenant's record (13 published tracked dispatches, 0 attributed fans → about
  0.07 per dispatch, was 2.0). The checkpoint rebuilds once (`EVIDENCE_BASIS_VERSION` 4).
  Expect fewer assumption-driven dispatches; `min_dispatches` still yields at least one
  per cycle while any candidate has positive value. Watch `ops/summary`; if the brain goes
  fully quiet, tell the loop.
- **Content-artifact lane is held when it is down** *(mine, #518)*: after 4 consecutive
  lane failures an artifact kind is held and one probe goes out per 1 h to 6 h backoff.
  Artifact production is no longer measured as fan growth (#520, #525), so pending
  `incremental_fan_growth_*` rows for artifacts stop appearing.
- **Unattended authority is harder to earn** (#525, #527, #529, #530): self-observation and
  neutral zeros cannot earn it; it needs positive external outcomes. Contexts will not gain
  autonomy from their own drafts.
- **Owned channel-less links land on the `/watch` capture page** *(mine, #533)* with
  `utm_source=owned`. Reddit communities that forbid links and links tied to a community are
  unchanged. Click counts are unchanged; the landing differs.
- **Releases join the community first-touch surge** *(mine, #523)* and the `listen_url` bug
  is fixed: expect more community drafts, still behind the existing community authority gate.
- **A peer is evidence, not reach** (#515, #517, #524): suggestions that promised a peer
  audience without an active consent are retired; approved ones are untouched.
- **Day-0 / executable rails** (#536 to #540, #545): owned-social cold-start work is emitted
  only to rails the owner explicitly granted. **Until the Facebook grant (section 5) the
  owned-social acquisition lane stays quiet by design.** Join-ask publication now needs a
  provider confirmation (#545).
- **Worker makes one new outbound call** *(mine, #541)*: every 6 h a read-only
  `graph.facebook.com/{v}/debug_token` to learn the publish token's scopes. It posts
  nothing and logs no token. If Graph does not answer for this token type, readiness shows a
  caveat and nothing else changes.
- **YouTube capture comments are prepared, not posted** *(mine, #535)*: up to 3 drafts appear
  in `ops/attention` under `unpublished_drafts.youtube.ready_to_post` with the exact words
  and the tracked link.
- **New read-only control-plane routes:** `/growth/readiness` (#536, extended in #541),
  `/growth/signup-channels` *(mine, #544)*, `/growth/prospect-identity-exclusions` (GET/POST/
  DELETE, #514). `ops/attention` gains `growth_readiness` *(mine, #542)* and the `youtube`
  channel in `unpublished_drafts`.

## 4. Already done in prod, before this deploy (owner-authorised, additive)

- `tenant_settings.scout_own_handles = 'wojciech_bator, WojciechBator'` (read by #513's sweep).
- One `scout_breach_acknowledgements` row clearing the false `over_rate` breach; the reply lane
  stopped halting at once (0 halts vs 20 per 10 min).
- Rollback: `DELETE FROM tenant_settings WHERE key='scout_own_handles'; DELETE FROM scout_breach_acknowledgements WHERE acknowledged_by LIKE 'claude on owner instruction%';`

## 5. Owner-only switches (not touched by code or by me)

The Facebook rail is one decision from autonomous. All three are needed, and no DB write can
supply the second:

1. Tenant authority: `social_auto_post = true` and add `facebook` to `social_autopost_platforms`
   (currently `false` / `telegram`). The explicit-grant handoff from #537 is the intended path.
2. Deployment gate: `CROWDRELAY_SOCIAL_AUTO_POST=true` on the worker, then restart it.
3. A token that can publish (`pages_manage_posts`). After deploy the preflight says so within
   6 h: `docker logs crowdrelay-worker-1 | grep "publish token scopes"`.

Also: after the migration, record the owner's own accounts as durable exclusions (the stronger
form of `scout_own_handles`), e.g. `POST /v1/control-plane/growth/prospect-identity-exclusions`
for `instagram` / `wojciech_bator` and `youtube` / `@wojciechbator` with reason `own_account`.

## 6. Proofs to run after deploy

```sh
ops/growth/verify-deploy.sh            # read-only; [ok]/[zero]/[warn]/[info]
```

| Line | Expect after deploy | If not |
| --- | --- | --- |
| prod is at origin/main | `[ok]` | the deploy is behind or rolled back |
| no `scout lane halted` | `[ok]` | a real breach: read `scout_lane` before acknowledging |
| owner's account prospects | `0` within an hour | sweep not running; check worker logs |
| `prior seeded` | `delivered=13 attributed=0 prior_mean_fans≈0.07` | the checkpoint did not rebuild |
| artifact failures in 2 h | `<=1` (a probe per backoff) | the breaker is not engaged |
| publish token | verified, or `could not be verified` (caveat, not a block) | `lacks pages_manage_posts` means reconnect the token |
| owned-landing signups | `[zero]` until someone joins | a join with `utm_source=owned` is the first real proof of #533 |
| joins by campaign tag | `(untagged)` only until a tagged join | an `owned/watch` row after the first one |
| push endpoints | active count rises with virya#56 | the ask is not visible; check My Signal |

A `[zero]` is a measured result, not a failure. An invented success is.
