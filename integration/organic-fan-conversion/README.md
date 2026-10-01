# Organic fan conversion delivery

This change repairs consented fan capture and uses retained fans, rather than raw attention, to guide organic growth. It reuses the existing signup, lifecycle, attribution and opportunity mechanisms. Human approval still owns relationships.

## One review, two repositories

GitHub cannot merge changes to `CrowdRelay/crowdrelay` and `CrowdRelay/virya` through one ordinary pull request. This CrowdRelay pull request therefore carries the backend changes and the complete website changes as `virya.patch`. Only one pull request is opened. Merging it does not merge or deploy the website automatically.

The website implementation is also available on `CrowdRelay/virya` branch `growth/organic-fan-conversion`:

- Base: `bb0c7e092ebfba723e2e1a2232ac9660868b126e`
- Commit: `6051a99f4dc65d51b0f7c098558c05a1eadc86cc`
- Compare: https://github.com/CrowdRelay/virya/compare/bb0c7e092ebfba723e2e1a2232ac9660868b126e...6051a99f4dc65d51b0f7c098558c05a1eadc86cc

The patch is generated with `git format-patch`. Applying it to the recorded base reproduces the website commit's complete Git tree. Use either the branch or the patch; do not apply both.

## Backend behavior

- `POST /v1/fans` accepts an omitted city. A supplied city still receives normal validation. Consent remains mandatory.
- Signup still commits the fan, consent, acquisition provenance, idempotency result and asynchronous email intent together. It never infers a city.
- Social evidence includes mature conversion cohorts and observed D30 retention. Rate comparisons require a complete observation window, at least four observed fans and reach of at least 100. Sparse or immature rates remain unmeasured.
- Community opportunities rank observed retained fans before acquired fans and traffic. Communities without measured posts retain null evidence.
- Referral invitations require a real ticket purchase, event interest or Synesthesia completion after signup. Signup age and a welcome receipt alone do not qualify.

## Website behavior

Signal, watch pages, concert check-in and AREA use the same consented durable signup helper. City enrichment is optional. City-loading failures no longer prevent Signal or watch-page signup. Timeout retries reuse the idempotency key for identical input. Visitor, campaign, referral and first-party landing attribution remain intact.

The obsolete preregistration relay is removed, including its pre-consent hashed-email Meta forwarding. Confirmation and recovery messages respect the backend's `email_queued` result. Known `offer=shows` and `offer=releases` values select truthful bilingual copy; arbitrary URL text is not rendered.

## Rollout

1. Merge and deploy the CrowdRelay backend first. No database migration is required.
2. Integrate the website branch through the website repository's normal protected-branch process, or apply the patch in a clean website checkout:

   ```sh
   git checkout -b growth/organic-fan-conversion bb0c7e092ebfba723e2e1a2232ac9660868b126e
   git am /absolute/path/to/crowdrelay/integration/organic-fan-conversion/virya.patch
   npm run check
   npm test
   npm run build
   ```

3. Deploy the website through its existing pipeline.
4. Smoke-test a new consented email with no city, confirm the inbox link, and inspect durable attribution. Verify unchecked consent prevents submission and retrying does not create a second acquisition.
5. Measure acquired, activated and retained fans by source. Repository tests do not establish production growth or actual email delivery.

## Rollback

Revert the website first, then the backend. Reversing that order makes the new no-city website requests incompatible with the old required-city API. Existing fans captured without a city remain valid rows; rollback must not delete them or their consent and attribution history.
