//! Keeping the outreach engine supplied with something to pitch and someone
//! to pitch it to.
//!
//! The outreach evaluator only ever sees `outreach_opportunities`, and those
//! were written in exactly two places: when a show's fan announcement
//! executed, and when a release milestone executed. Both are one-shot. In
//! production the two announcements ran on 2026-08-25 and 08-27, before the
//! act's 399 outreach contacts were imported, so the engine held six
//! opportunities for three contacts, due to expire on 2026-09-26, and never
//! proposed a pitch to anyone else. The letter had the same one-shot flaw:
//! it pitched only a release plan with a listen link, the workspace had none,
//! and every draft came out empty while 26 releases sat in `content_sources`.
//!
//! This runs every cycle and keeps the supply current:
//! - **the pitch** — the newest active release plan with a listen link, or,
//!   without one, the newest album, EP or single in the synced catalogue
//!   whose release date is known (a synced item's `occurred_at` is the sync
//!   time, not the release, so it cannot rank a catalogue);
//! - **the pitch's audience** — every writable press, radio, creator,
//!   patronage and endorsement contact nobody has served yet gets an
//!   opportunity for the pitch (the wave kinds; playlists have their own
//!   placement phase). A catalogue opportunity is pitched only inside a
//!   monthly catalogue wave (`WaveAnchor::Catalogue`), never as a loose card;
//! - **the next shows** — every published show from three to sixty days out
//!   gets the same announcement-shaped opportunities, expiring two days
//!   before the night;
//! - **retirement** — opportunities for a pitch that is no longer the pitch,
//!   or a show that is no longer published and ahead, stop being active.
//!
//! Nothing here sends anything. It is the evaluator that decides, per
//! contact, whether a first letter or a follow-up is due, under the outreach
//! policy's approval, caps, silence rules and the contact governor.

use super::*;

/// Opportunities are refreshed every cycle, so this is how long one survives
/// if the cycles stop.
const CATALOGUE_OPPORTUNITY_DAYS: i32 = 30;
/// A show is pitched from sixty days out until three days before; the
/// opportunity itself closes two days before the night.
const SHOW_PITCH_FROM_DAYS: i32 = 60;
const SHOW_PITCH_UNTIL_DAYS: i32 = 3;

/// Which contacts a show is news to, over the aliases `target` and `event`.
///
/// A show opportunity used to be written for every press, radio, creator and
/// patronage contact in the registry, wherever they were: the Gorzów show on
/// 2026-10-17 queued letters to a festival in Bratislava, a youth contest in
/// Berlin and a site in Kufstein. The registry holds one fact about where a
/// contact is — the domain of their address — so a show is offered to the
/// contacts whose address is under the show city's own country domain (the
/// ISO code is the ccTLD for every country the act has played). A free-mail
/// address says nothing about where its owner is and is left out; the
/// catalogue pitch still reaches it. Organisers are asked for a slot, not
/// about this night, and are not filtered.
pub(crate) const SHOW_LOCAL_TARGET: &str = "(target.target_kind = 'organiser' OR EXISTS (\
    SELECT 1 FROM cities AS show_city \
    WHERE show_city.id = event.city_id \
      AND show_city.country_code IS NOT NULL \
      AND lower(split_part(target.contact_email, '@', 2)) \
          LIKE '%.' || lower(show_city.country_code)))";

/// What the act is pitching, and the subject its opportunities are keyed by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutreachPitch {
    pub subject_kind: &'static str,
    pub subject_key: String,
    pub source: &'static str,
    pub title: String,
    pub url: String,
    /// The release plan the pitch is, when it is one — its listen link mints
    /// campaign-bound through `ensure_release_tracked_link`, not bare.
    pub release_id: Option<crowdrelay_domain::ReleasePlanId>,
    /// The plan's own `source_key`, when the pitch is a plan — the key
    /// `release_link_slug` names the canonical link from, so the letter
    /// carries the same link the release milestones printed.
    pub source_key: Option<String>,
}

impl OutreachPitch {
    /// The tracked link a letter prints for this pitch.
    ///
    /// A plan pitch carries the release's canonical `release-{key}` link — the
    /// same row the release milestones mint, so clicks from the letter and
    /// clicks from the announcement aggregate on one link and one campaign. A
    /// catalogue pitch gets `release-catalogue-{id}` instead. `Ok(None)` —
    /// the letter then refuses or shortens rather than printing a URL the
    /// ledger cannot see — when the tenant has no member site or the listen
    /// URL is not a safe redirect target.
    pub(crate) async fn tracked_link(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        workspace_id: Uuid,
        site_root: Option<&str>,
    ) -> Result<Option<crowdrelay_domain::TrackedLink>, RepositoryError> {
        let key = self.source_key.as_deref().unwrap_or(&self.subject_key);
        let Some(slug) = operations::release_link_slug(key)
            .and_then(|slug| crowdrelay_domain::SmartLinkSlug::parse(slug).ok())
        else {
            return Ok(None);
        };
        if let Some(release_id) = self.release_id {
            // `ensure_release_tracked_link` no-ops on a non-http listen URL —
            // a letter must never print a link whose row was never written,
            // so the same gate runs here before the link is built.
            if !operations::is_http_url(&self.url) {
                return Ok(None);
            }
            // Campaign-bound: the release's own mint path keeps the link the
            // milestones' audiences already click.
            operations::ensure_release_tracked_link(
                transaction,
                crowdrelay_domain::WorkspaceId::from_uuid(workspace_id),
                release_id,
                key,
                &self.title,
                Some(&self.url),
            )
            .await?;
            return Ok(site_root.map(|root| crowdrelay_domain::TrackedLink::for_site(root, &slug)));
        }
        crate::tracked_links::ensure_smart_link_in_tx(
            transaction,
            workspace_id,
            slug.as_str(),
            &self.url,
            site_root,
            Some("email"),
            Some("pitch"),
        )
        .await
        .map_err(map_sqlx)
    }
}

/// The pitch every outreach letter carries: an operator's release plan with
/// a listen link wins; otherwise the newest dated album, EP or single in the
/// synced catalogue. `None` when the workspace has neither.
pub(crate) async fn outreach_pitch(
    connection: &mut sqlx::PgConnection,
    workspace_id: Uuid,
) -> Result<Option<OutreachPitch>, RepositoryError> {
    let plan = sqlx::query_as::<_, (Uuid, String, String, String)>(
        "SELECT id, title, listen_url, source_key FROM release_plans
         WHERE workspace_id = $1 AND active
           AND listen_url IS NOT NULL AND btrim(listen_url) <> '' AND btrim(title) <> ''
         ORDER BY release_at DESC, id
         LIMIT 1",
    )
    .bind(workspace_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_sqlx)?;
    if let Some((id, title, url, source_key)) = plan {
        return Ok(Some(OutreachPitch {
            subject_kind: "release",
            subject_key: format!("release:{id}"),
            source: "release_autopilot",
            title,
            url,
            release_id: Some(crowdrelay_domain::ReleasePlanId::from_uuid(id)),
            source_key: Some(source_key),
        }));
    }
    let catalogue = sqlx::query_as::<_, (Uuid, String, String)>(
        r#"
        SELECT id, title, btrim(metadata->>'url')
        FROM content_sources
        WHERE workspace_id = $1
          AND source_kind = 'release'
          AND metadata->>'release_type' IN ('album', 'ep', 'single')
          AND metadata->>'released_at' ~ '^[0-9]{1,12}$'
          AND COALESCE(btrim(metadata->>'url'), '') ~* '^https?://'
          AND btrim(title) <> ''
        ORDER BY (metadata->>'released_at')::bigint DESC, id
        LIMIT 1
        "#,
    )
    .bind(workspace_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(map_sqlx)?;
    Ok(catalogue.map(|(id, title, url)| OutreachPitch {
        subject_kind: "catalogue",
        subject_key: format!("catalogue:{id}"),
        source: "catalogue_autopilot",
        title,
        url,
        release_id: None,
        source_key: None,
    }))
}

/// What one refresh did — logged by the worker, asserted by the tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutreachSupplyRefresh {
    /// The pitch's title, when there is one.
    pub pitch: Option<String>,
    /// Opportunity rows written or re-activated this cycle.
    pub opportunities_live: u64,
    /// Opportunity rows retired this cycle.
    pub opportunities_retired: u64,
}

/// The contacts an opportunity may be written for: writable, verified,
/// accepting outreach, never served. The kind is filtered per subject; agents,
/// labels, support slots and playlists have their own paths.
const ELIGIBLE_TARGET: &str = "
    target.active AND target.verified AND target.accepts_outreach
    AND NOT target.do_not_contact
    AND COALESCE(target.last_reply_disposition::text, 'none') NOT IN ('received', 'positive', 'declined')
";

impl PostgresAutopilotRepository {
    /// Runs one supply refresh for the workspace. See the module docs.
    pub async fn refresh_outreach_supply(
        &self,
        workspace_id: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<OutreachSupplyRefresh, RepositoryError> {
        self.bounded(async {
            let ws = workspace_id.into_uuid();
            let mut transaction = self.pool.begin().await.map_err(map_sqlx)?;
            let pitch = outreach_pitch(&mut transaction, ws).await?;
            let mut report = OutreachSupplyRefresh {
                pitch: pitch.as_ref().map(|pitch| pitch.title.clone()),
                ..OutreachSupplyRefresh::default()
            };

            if let Some(pitch) = &pitch {
                let written = sqlx::query(&format!(
                    r#"
                    INSERT INTO outreach_opportunities(
                        workspace_id, target_id, source, subject_kind, subject_key, template_key,
                        relevance_basis_points, confidence_basis_points, active, observed_at, expires_at)
                    SELECT target.workspace_id, target.id, $2, $3, $4,
                           $3 || '.' || CASE target.target_kind
                               WHEN 'media_patronage' THEN 'media_patronage'
                               WHEN 'endorsement' THEN 'endorsement'
                               ELSE 'press'
                           END || '.v1',
                           GREATEST(7000, LEAST(10000, target.relationship_score * 100)), 8500,
                           true, $5, $5 + make_interval(days => $6)
                    FROM outreach_targets AS target
                    WHERE target.workspace_id = $1
                      AND {ELIGIBLE_TARGET}
                      AND target.target_kind IN
                          ('press', 'radio', 'creator', 'media_patronage', 'endorsement')
                    ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key) DO UPDATE SET
                        active = true, observed_at = EXCLUDED.observed_at,
                        expires_at = EXCLUDED.expires_at
                    "#
                ))
                .bind(ws)
                .bind(pitch.source)
                .bind(pitch.subject_kind)
                .bind(&pitch.subject_key)
                .bind(now)
                .bind(CATALOGUE_OPPORTUNITY_DAYS)
                .execute(&mut *transaction)
                .await
                .map_err(map_sqlx)?;
                report.opportunities_live += written.rows_affected();
            }

            // The next shows: the same opportunity the announcement writes,
            // kept current for contacts imported after it ran.
            let shows = sqlx::query(&format!(
                r#"
                INSERT INTO outreach_opportunities(
                    workspace_id, target_id, source, subject_kind, subject_key, template_key,
                    relevance_basis_points, confidence_basis_points, active, observed_at, expires_at)
                SELECT target.workspace_id, target.id, 'event_autopilot', 'event',
                       'event:' || event.id::text,
                       CASE target.target_kind
                           WHEN 'media_patronage' THEN 'event.media_patronage.v1'
                           WHEN 'endorsement' THEN 'event.endorsement.v1'
                           WHEN 'organiser' THEN 'event.organiser.v1'
                           ELSE 'event.press.v1'
                       END,
                       GREATEST(7000, LEAST(10000, target.relationship_score * 100)), 8800,
                       true, $2, event.starts_at - interval '2 days'
                FROM events AS event
                JOIN outreach_targets AS target ON target.workspace_id = event.workspace_id
                WHERE event.workspace_id = $1
                  AND event.status = 'published'
                  AND event.starts_at > $2 + make_interval(days => $4)
                  AND event.starts_at <= $2 + make_interval(days => $3)
                  AND {ELIGIBLE_TARGET}
                  AND {SHOW_LOCAL_TARGET}
                  AND target.target_kind IN ('press', 'radio', 'creator', 'media_patronage', 'endorsement', 'organiser')
                ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key) DO UPDATE SET
                    active = true, observed_at = EXCLUDED.observed_at,
                    expires_at = EXCLUDED.expires_at
                "#
            ))
            .bind(ws)
            .bind(now)
            .bind(SHOW_PITCH_FROM_DAYS)
            .bind(SHOW_PITCH_UNTIL_DAYS)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            report.opportunities_live += shows.rows_affected();

            // The threads the act started by hand. The sheet import never
            // linked those messages to an opportunity, so without this seed
            // the relationship cooldown holds them for ever — 111 contacts
            // in production were waiting on an answer no engine would chase.
            // One opportunity per thread, keyed on the target: the unique
            // constraint is what makes the seed idempotent across cycles,
            // and the lane's own one-nudge cap stops a reactivated row from
            // ever sending twice.
            let threads = sqlx::query(&format!(
                r#"
                INSERT INTO outreach_opportunities(
                    workspace_id, target_id, source, subject_kind, subject_key, template_key,
                    relevance_basis_points, confidence_basis_points, active, observed_at, expires_at)
                SELECT target.workspace_id, target.id, 'thread_followup', 'thread',
                       'thread:' || target.id::text, 'outreach.thread.v1',
                       GREATEST(7000, LEAST(10000, target.relationship_score * 100)), 8000,
                       true, $2, last_out.occurred_at + interval '60 days'
                FROM outreach_targets AS target
                JOIN LATERAL (
                    SELECT message.occurred_at
                    FROM outreach_interactions AS message
                    WHERE message.workspace_id = target.workspace_id
                      AND message.target_id = target.id
                      AND message.direction = 'outbound'
                      AND message.opportunity_id IS NULL
                      AND message.occurred_at <= $2
                      AND NOT EXISTS (
                          SELECT 1 FROM outreach_interactions AS later
                          WHERE later.workspace_id = target.workspace_id
                            AND later.target_id = target.id
                            AND later.occurred_at > message.occurred_at
                      )
                    ORDER BY message.occurred_at DESC, message.id DESC
                    LIMIT 1
                ) AS last_out ON true
                WHERE target.workspace_id = $1
                  AND {ELIGIBLE_TARGET}
                  AND target.target_kind IN
                      ('playlist', 'radio', 'press', 'creator', 'support_slot', 'endorsement', 'media_patronage', 'organiser')
                  AND last_out.occurred_at > $2 - interval '60 days'
                ON CONFLICT (workspace_id, source, target_id, subject_kind, subject_key) DO UPDATE SET
                    active = true, observed_at = EXCLUDED.observed_at,
                    expires_at = EXCLUDED.expires_at
                "#
            ))
            .bind(ws)
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            report.opportunities_live += threads.rows_affected();

            // Retire what no longer stands: a catalogue pitch that is not the
            // current one, a show that is cancelled, moved into the past or
            // no longer published, and a thread that stopped being the act's
            // unanswered handwritten message — they answered, the operator
            // wrote again, the contact went do-not-contact, or the nudge
            // itself already went out, which also makes the latest message a
            // linked one. A release plan's own rows are left to the release
            // path, which owns their window.
            let retired = sqlx::query(&format!(
                r#"
                UPDATE outreach_opportunities AS opportunity
                SET active = false, updated_at = now()
                WHERE opportunity.workspace_id = $1
                  AND opportunity.active
                  AND (
                      (opportunity.source = 'catalogue_autopilot'
                       AND opportunity.subject_key IS DISTINCT FROM $2)
                      OR (opportunity.source = 'event_autopilot'
                          AND EXISTS (
                              SELECT 1
                              FROM outreach_targets AS target
                              JOIN events AS event
                                ON event.workspace_id = target.workspace_id
                               AND 'event:' || event.id::text = opportunity.subject_key
                              WHERE target.workspace_id = opportunity.workspace_id
                                AND target.id = opportunity.target_id
                                AND NOT {SHOW_LOCAL_TARGET}
                          ))
                      OR (opportunity.source = 'event_autopilot'
                          AND NOT EXISTS (
                              SELECT 1 FROM events AS event
                              WHERE event.workspace_id = $1
                                AND 'event:' || event.id::text = opportunity.subject_key
                                AND event.status = 'published'
                                AND event.starts_at > $3
                          ))
                      OR (opportunity.source = 'thread_followup'
                          AND NOT EXISTS (
                              SELECT 1
                              FROM outreach_targets AS target
                              JOIN LATERAL (
                                  SELECT message.occurred_at
                                  FROM outreach_interactions AS message
                                  WHERE message.workspace_id = target.workspace_id
                                    AND message.target_id = target.id
                                    AND message.direction = 'outbound'
                                    AND message.opportunity_id IS NULL
                                    AND message.occurred_at <= $3
                                    AND NOT EXISTS (
                                        SELECT 1 FROM outreach_interactions AS later
                                        WHERE later.workspace_id = target.workspace_id
                                          AND later.target_id = target.id
                                          AND later.occurred_at > message.occurred_at
                                    )
                                  ORDER BY message.occurred_at DESC, message.id DESC
                                  LIMIT 1
                              ) AS last_out ON true
                              WHERE target.workspace_id = opportunity.workspace_id
                                AND target.id = opportunity.target_id
                                AND {ELIGIBLE_TARGET}
                                AND target.target_kind IN
                                    ('playlist', 'radio', 'press', 'creator',
                                     'support_slot', 'endorsement', 'media_patronage',
                                     'organiser')
                                AND last_out.occurred_at > $3 - interval '60 days'
                          ))
                  )
                "#
            ))
            .bind(ws)
            .bind(
                pitch
                    .as_ref()
                    .filter(|pitch| pitch.source == "catalogue_autopilot")
                    .map(|pitch| pitch.subject_key.clone()),
            )
            .bind(now)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx)?;
            report.opportunities_retired = retired.rows_affected();

            transaction.commit().await.map_err(map_sqlx)?;
            Ok(report)
        })
        .await
    }
}

/// Whether an outreach action under a show opportunity carries a letter that
/// was not composed as the show letter. Organisers are asked for a slot and
/// keep the organiser letter under either key.
pub(crate) fn stale_show_letter(opportunity_template_key: &str, action_template_key: &str) -> bool {
    opportunity_template_key.starts_with("event.")
        && opportunity_template_key != "event.organiser.v1"
        && action_template_key != opportunity_template_key
}

#[cfg(test)]
mod stale_show_letter_tests {
    use super::stale_show_letter;

    #[test]
    fn an_album_pitch_under_a_show_opportunity_is_stale() {
        assert!(stale_show_letter("event.press.v1", "outreach.press.v1"));
        assert!(stale_show_letter(
            "event.media_patronage.v1",
            "outreach.media_patronage.v1"
        ));
        assert!(!stale_show_letter("event.press.v1", "event.press.v1"));
        assert!(!stale_show_letter(
            "event.organiser.v1",
            "outreach.organiser.v1"
        ));
        assert!(!stale_show_letter(
            "catalogue.press.v1",
            "outreach.press.v1"
        ));
    }
}
