# Organic fan loop acceptance

This follow-up starts at CrowdRelay `1e1ad4c5f8bb5614200f278b05353762a6139c52`.
It incorporates concurrent main `dc139df20f4903aac5ccb69c39018716c6e98e9e`
without changing its relationship research or operator-lane safety work.
The acquisition context, deliberate engagement funnel, bounded publication recovery
and install-template approval implementation already landed in #443. This package
finishes the recall attribution and regression gaps; it does not replay that work.

## Website dependency

VIRYA #52 fixes the inline concert signup's missing `slug` binding, the capture-context
type and the merged site's CI budgets. Keep that separate repair. The accompanying
`virya.patch` adds only a compiler-backed concert binding regression, based on #52's
exact head `5adbba28b4e75d6a2c4a5227e01df7ab9ba5079f`.

The review branch `CrowdRelay/virya: growth/fix-concert-capture-context` incorporates
#52 unchanged plus that regression. Merging the CrowdRelay PR alone does not update
the separate website. Apply the patch after the website repair:

```sh
git apply --check /path/to/virya.patch
git apply /path/to/virya.patch
npm test
npm run build
```

The regression checks the actual `EventDetail.tsx` with TypeScript's binder. It fails
on the old merged code with `Cannot find name 'slug'` and passes on #52. The regular
site typecheck remains authoritative for imported types and the rest of the app.

## Runtime behavior

- Each new show recall has its own `show-recall-{action_id}` redirect and durable
  `smart_links.action_id`. Two recipients of the same show no longer share credit.
  Historical shared redirects and observations are not rewritten.
- A fan who opens Signal after the recall was proposed still receives the approved
  thank-you, but its stale installation CTA is omitted at dispatch. A standalone
  install ask retains its existing refusal when no longer needed.
- Install dispatch regressions start with consented active fans, an enabled policy
  and a configured site. A positive emission control prevents unrelated setup
  errors from making every refusal test pass. Refusal cases assert the exact reason
  and zero outbox emissions/links; no message is actually sent by these tests.

No API schema or database migration changes. The external executor uses the complete
payload links verbatim, accepts absent `fan.install_url` on recalls, and keeps the
existing fresh-consent check before delivery. Dispatch checks do not prove delivery.

## Verification recorded in this run

- 18 acquisition/context/confirmation tests passed on the recovered website snapshot.
- The new binding regression passed against #52 and failed against the old `main`
  component with the expected missing `slug` error.
- The combined available suite passed 19/19. SQL syntax checks passed for 23 changed
  query literals; these checks do not validate the migrated schema or execute Rust.
- The changed Rust files parsed/formatted with actual rustfmt on WASM. Changed source
  sizes remain within the repository ratchet; whitespace and patch application checked.
- Added PostgreSQL coverage for positive emission, revocation, expiry, Observe,
  Recommend, disabled policy, installed fan, withdrawn consent, replay protection,
  distinct recall ownership and installation after the recall claim.
- Native Cargo/PostgreSQL execution and a full website build were unavailable in
  this partial checkout (no native Rust toolchain/test database/full site dependencies).
  These are required release checks, not claimed passes. Production `/v1/meta` and
  `/metrics` requests timed out at the proxy; the running revision is unverified.
- Main's nightly PostgreSQL job `110743360617` failed. Its existing install-grant
  fixture claimed zero actions instead of two. This PR supplies its missing consent,
  enabled policy and site setup and adds a positive control; a native rerun is still
  required. The other nightly failures and CI workflow repairs are outside this PR.

Run the PostgreSQL module with the repository's disposable migrated test database,
using its existing `CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL` convention, and run the
normal required checks. Do not substitute syntax validation for those checks.

## One real organic acceptance after an authorized deployment

Use the next real organic publication with an action-owned tracked join link and
an actual published event/music resource. Resolve its current slug from tenant data;
do not invent a Gorzow date, prize, asset or fan. Reuse the existing join/show placement
kits and their approvals. This code task authorizes no publication or outreach.

1. Record the running API/worker and website revisions. Verify migrations 0396–0398
   and the website confirmation flow through normal deployment gates.
2. Keep the publication's provider receipt and its own tracked link/action IDs.
   An executor acknowledgement alone is not a publication receipt.
3. Read `/v1/admin/ops/organic-funnel?action_id=<id>&days=90` with the existing private
   tenant authentication. Inspect publication verification and ownership before
   interpreting traffic. Browser IDs can include link previews.
4. Let real visitors decide to sign up and explicitly consent. Confirmation must
   return to the same event/music resource, including on another device. A retry
   reuses the durable confirmation operation; it must not request extra mail.
5. Observe a deliberate fan action (interest, genuine check-in, nonsynthetic completed
   experience, purchase, or qualified referral). An automatic session/installation
   alone is not activation. Returning to an event page alone does not fake a door scan.
6. After seven days, compare `activated_mature` with `activation_mature`. Before D30,
   retention is immature. Later use `retained` with `retention_mature` and separately
   inspect qualified referrals. Do not sum parent/child observations as unique fans
   or call an attributed count causal lift.
7. For approved T+1 recalls, verify distinct redirects per action and no installation
   CTA for an already identified install. Verify an actual delivery receipt separately.

If there is no verified publication, fix delivery/coverage. If verified publication
gets no real visits, fix placement and audience relevance. If visits do not confirm,
inspect promise/form/inbox friction. If mature confirmed fans do not engage, improve
the promised resource and relevant follow-up. Zero and immature outcomes are evidence;
they do not justify broader contact permission or repeated unapproved sends.
