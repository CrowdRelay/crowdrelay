# Intake-to-fan verification

The goal is more real, engaged fans for each tenant, followed by ticket purchases,
merch purchases, and attendance. Imported contacts, generated content, and successful
autopilot actions are intermediate state, not evidence of that goal.

**Standing mandate (owner directive, 2026-10-02):** ≥100 system-attributed new
fans EVERY month (October 2026 first; prod objective `signal/active_fans`
20 → 120 by 2026-10-31) and every promoted video ≥1,000 organic views /
≥10 likes / ≥5 comments via CrowdRelay operations. A fan counts only when a
`fan_provenance_events`/`fan_acquisition_events` row traces them to a CrowdRelay
action — manual invites and imports do not count. Working plan:
`~/.devin/plans/FAN_100_PLAN.md`.

## Reproduce the read-only snapshot

Use a database role with read access. Do not put a database password in a report or
shell history. Run from the repository root:

```sh
psql -X "$DATABASE_URL" -v ON_ERROR_STOP=1 \
  -v workspace_slug=virya -v intake_revision=7 \
  -f ops/growth/verify-intake.sql
```

Set `intake_revision` to `SHEET_INTAKE_REVISION` in the **deployed** worker. Compare
its image revision with the release being verified. The report uses a read-only
transaction, tenant-scoped queries, and bounded statement/lock timeouts. It excludes
email addresses, credentials, and message bodies. File names can still be sensitive;
keep production output private.

The report fails for an unknown workspace. Stored file markers include files that
were scanned earlier and may no longer be in the connection's current scope. A
current marker proves the parser completed that stored version, not that every row
was eligible or that the source is still authoritative. File row counts cover all
tabs; the per-sheet row bound cannot be inferred from the workbook total.

## Release acceptance

1. Ship through the existing CI, image publication, and ecosystem deployment gates.
   Do not bypass a failed gate or deploy an unmerged PR.
2. Verify the running API and worker revisions and health after deployment.
3. Verify the Drive scope. Prefer the authoritative registry folder when practical.
   Do not delete archives or change scope without checking which inputs it excludes.
4. Capture the report and the next Drive cycle's counters. A parser revision change
   invalidates old unchanged markers; an existing scan trigger can wake the worker.
5. Reconcile each authoritative workbook: imported/refreshed rows, validation
   refusals, unmatched history, write failures, and refused exports. Split or trim a
   workbook refused by Google's 10 MB export limit. Repeatedly waking the scan does
   not fix an unchanged refused version.
6. Confirm a second unchanged scan does not duplicate opportunities, beacons,
   contacts, or outreach interactions, and does not reset existing decisions.

The PostgreSQL `sheet_intake` tests run in both CI and `just test-postgres`. They
cover tab routing, banners, cross-dialect deduplication, preservation of opportunity
status, contact ownership, and defanging untrusted attachments. These are synthetic
regression proofs; they do not replace reconciliation of current Drive contents.

Partial Drive scans retain a connection error. Previously refused unchanged files
remain visible without downloading again. Failed registry writes do not seal an
unchanged marker, including festival edition writes. Parser refusals and unmatched
history are review outcomes, not transient provider failures.

## Resolve the existing backlog safely

- Separate fans from venues, promoters, press, radio, festivals, and booking agents.
  Use the existing contact review and promotion surfaces; do not auto-promote every
  spreadsheet address into a fan.
- Preserve source provenance, suppressions, refusal dates, and do-not-contact state.
  Spreadsheet presence is not marketing consent. Public contact verification is
  not proof that outreach is accepted.
- Reconcile outreach log recipients with known targets before follow-ups. Otherwise
  the system can contact someone whose prior reply is still unmatched.
- Review duplicate identities and conflicting source decisions. Email deduplication
  alone does not resolve stale backups or conflicting statuses.
- Select a small number of opportunities with verified destinations, open deadlines,
  audience fit, acceptable costs, and available artist capacity. Do not mass-submit
  the inventory or treat expired opportunities as current calls.

## Prove one complete campaign

Use a real upcoming event or release and one suitable community or partner. Keep
all external actions within existing approvals, standing grants, consent, and rate
limits. Do not bypass a moderator-removal hold. Do not invent anecdotes or claim
first-hand experience. Missing platform credentials require a legitimate operator
connection or a disabled lane, not repeated failed joins.

For each stage, record its durable receipt:

| Stage | Required evidence |
| --- | --- |
| Publish | Provider post ID/URL or delivered partner message, not only action success |
| Discover | Tracked link interaction with campaign/channel identity |
| Join | Deduplicated fan identity, arrival provenance, and explicit consent where required |
| Activate | The existing activation event and its attribution to the acquisition cohort |
| Re-engage | Relevant interaction or delivered notification; staff receipts are not fan receipts |
| Convert | Real payment receipt or attendance/check-in; exclude synthetic/test activity |
| Retain | A later return interaction in the configured retention window |

Use existing smart links and acquisition/attribution logic. Do not create a parallel
metrics system. The acquisition event's `source=public_signup` is an entry surface,
not proof that channel attribution is absent: inspect campaign linkage and the
existing interaction/provenance/attribution records.

Evaluate activated and retained fans by acquisition cohort, conversions, costs,
unsubscribes, and moderator removals. Keep follower counts and imported inventory
as supporting observations. A campaign can pass delivery verification while failing
to attract a fan; record that measured zero rather than claiming success.

## Production baseline observed on 2026-09-30

The read-only audit around 07:25 UTC found production on `55e9a306`, with revision-6
file markers; the scout routing changes on `c4f649f1` were not yet running. Drive
considered 38 files in the latest cycle and skipped all as unchanged. The connection
reported working, despite a stored export refusal. Changed scans reported unmatched
outreach history that still required review.

The tenant had 2,813 staged contacts, 20 active fan records, 14 signups and 11 activated
fans in the 30-day KPI, 18 reachable consented fans, and two active fan push endpoints.
There were four posted community rows in the seven-day window and 13 held rows after
moderator removals. One ticket order was marked paid; its live-versus-test status was
not verified. These observations are a baseline, not a completed growth experiment.

Retention and real conversion require elapsed time and real audience behavior.
Neither synthetic tests nor a PR can establish those outcomes in advance.
