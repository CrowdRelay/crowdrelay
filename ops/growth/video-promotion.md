# Guarded video promotion

Promotion uses the existing content-source, drop-surge, community relay, smart-link, and approval machinery. Requesting a cycle is not publishing a post, producing an artifact, or acquiring a fan.

## Platform policy

The source owns `metadata.promotion_excluded_platforms`. Videos and their recognized YouTube release projections without an explicit policy exclude Facebook and Instagram. An explicit empty array permits them. Invalid policy values exclude every recognized promotion platform. Other source kinds retain their existing defaults.

The existing authenticated promote endpoint accepts an optional JSON body:

```json
{"excluded_platforms": ["facebook", "instagram"]}
```

It validates platform names, persists exclusions across matching active video/release aliases in the workspace, stamps the operator's promotion request, and notifies the existing guarded cycle in one transaction. Alias matching uses the registered YouTube identity, never the title. Its `lanes` response describes requested work, not delivery. The console's material panel exposes the same operation with platform checkboxes. Existing callers without a body remain valid.

Selection, source-specific artifact requests, outcome ingestion, and first-party publishing recheck policy. External artifact producers receive the explicit exclusions and source metadata; they must honor those restrictions before publishing. A notification service that cannot produce an artifact must not report its message receipt as artifact delivery.

## Communities and actual readiness

The bounded video community rotation includes Reddit and joined forums, Lemmy, Telegram, and Discord communities. A release-plan source is not substituted for the registered video in a community dispatch; its owned-channel lanes remain available. Non-Reddit targets need an active place, joined membership, and a destination. Known promotion prohibitions and refused places are excluded. An existing draft or moderation hold consumes that target's turn; backlog backpressure is per platform so held Reddit work cannot starve a ready forum.

Each video dispatch names exactly one source and one target. Agents cannot silently substitute a newer video or another community. The worker uses the target's authoritative platform, not the model's proposed platform.

A send additionally requires current membership, a valid destination, verified rules that permit promotion, and an active credential for the actual publisher. Credential presence does not prove a live login. The sender still has to obtain a real provider receipt. Missing credentials or verification create manual work; withdrawn membership and explicit promotion prohibitions refuse the delivery. Discord community drafts remain manual-only. No Lemmy account, forum login, membership, flair, or moderator permission is fabricated.

Existing approval, consent, suppression, publisher review, rate limits, and Reddit standing remain authoritative. In particular, do not remove the Reddit moderator-removal hold or reapprove a stalled batch just to make it move.

## Evidence and attribution

A `content.artifact.request` success report requires a nonblank string in `metadata.artifact_delivery.url`, `.surface`, or `.reference`. An internal Discord notification or provider reference without that evidence records `artifact_delivery_missing`. Historical request-success rows without delivery evidence remain in the audit ledger but do not count as confirmed artifacts in the source panel or success evidence.

Artifact confirmation is not proof of public publication. Public community delivery requires the community ledger's real posted receipt. Clicks are interactions, not people or fans; never add outer and inner redirect counts or count anonymous provenance as fan acquisition.

Owned and community lanes share a source-owned acquisition campaign, reusing a matching release campaign where possible. Communities receive one tracked link directly to the registered source URL, not another `/l/` redirect. Links whose old community action proves exact source ownership can gain missing campaign attribution without changing their public slug. Existing event history is not rewritten into historical conversions or fans.

## Connector preservation and rollback

Migration `0383_source_owned_promotion_metadata.sql` adds a trigger preserving omitted promotion exclusions, campaign identity, and the promotion-request timestamp when older video or release producers refresh facts. Newly registered YouTube aliases inherit an existing asset's explicit exclusions. Explicit values still replace those fields. It does not modify existing rows or remove audit data. The video watcher merges refreshed facts so an unchanged feed does not bump versions merely because operator-owned metadata exists.

For a coordinated rollback, stop affected promotion workers first, restore compatible application versions through the normal deployment gates, and remove the additive trigger and function:

```sql
DROP TRIGGER content_sources_preserve_promotion_metadata ON content_sources;
DROP FUNCTION crowdrelay_preserve_source_promotion_metadata();
DROP FUNCTION crowdrelay_promotion_video_key(jsonb);
```

Do not delete source metadata, campaigns, links, or event history. Without the trigger, older fact-replacement writers can erase promotion settings again. Re-enable promotion only after checking source policy and ownership. The PostgreSQL proof exercises DDL and data rollback inside a disposable transaction.

## Production acceptance after gated deployment

1. Deploy the paired CrowdRelay, agents, and console changes through normal reviewed gates. Do not deploy an unmerged branch.
2. Open the registered video source and request promotion with Facebook and Instagram excluded. Do not re-create the source or reapprove the held Reddit batch.
3. Confirm that eligible joined forum and Discord targets receive source-pinned drafts. Check each community's actual permitted promotion location and rules. Complete missing credentials or manual publication legitimately.
4. Require public posting receipts, confirmed artifact evidence, and direct campaign-linked destinations. Unavailable producers must remain visibly incomplete.
5. Reconcile campaign clicks, distinct visitor identifiers, actual attributed fan identifiers, consented signup, activation, retention, and conversion using the existing acquisition ledger and growth verification report. No synthetic signup or notification-only success satisfies this step.

Local gates include real PostgreSQL proofs for platform isolation, request-receipt rejection, source policy changes after queuing, campaign reuse, legacy link repair, connector preservation, rollback, and credential/rule preflight without external sends.


## Owned fan capture on fresh YouTube uploads

A fresh YouTube upload is both a discovery asset and an acquisition surface. When the tenant has:
- a connected `youtube_account` grant,
- standing social auto-post approval,
- a configured member-site root, and
- at least one operator-written join-ask variant,

the YouTube worker posts one top-level comment on a fresh owned upload using that join-ask text plus a first-party tracked link to `/signal`.

The link belongs to the source's promotion campaign and is tagged as `channel_source=youtube`, `channel_community=video:{video_id}`, `channel_creative=fan_capture_comment`. A click therefore enters the normal anonymous visitor ledger; if that visitor later signs up, the acquisition can be credited back to the source campaign and YouTube channel instead of stopping at a view/click metric.

The worker does **not** edit video metadata, invent copy, or add a second CTA when the source description already contains `/signal`. Posting is capped at four capture comments per workspace per 24 hours and three attempts per video. Claim metadata on `content_sources` makes concurrent workers fail closed; stale claims reopen after two hours.
