//! The outreach engine's supply, end to end against a real Postgres.
//!
//! Production on 2026-09-25 held 399 outreach contacts and six outreach
//! opportunities: the only writers were a show's fan announcement and a
//! release milestone, both one-shot, and both ran before the contacts were
//! imported. The letter pitched only a release plan with a listen link, and
//! there was none, so even those six produced empty drafts. What only a real
//! schema proves:
//! - the refresh writes opportunities for the eligible contacts and no
//!   others, for the catalogue pitch and for the next shows, and retires them
//!   when the pitch changes or the show is cancelled;
//! - a full evaluation turns catalogue opportunities into one press wave of
//!   composed letters naming the pitch, and never into loose approval cards.

use std::time::Duration;

use crate::common;
use crowdrelay_application::autopilot::EvaluateAutopilot;
use crowdrelay_domain::{OutreachTargetId, WorkspaceId, outreach::OutreachReplyDisposition};
use crowdrelay_infra::{autopilot::PostgresAutopilotRepository, config::DatabaseConfig};
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) struct Fixture {
    pub(crate) pool: sqlx::PgPool,
    pub(crate) repository: PostgresAutopilotRepository,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) now: OffsetDateTime,
}

pub(crate) async fn fixture(label: &str) -> Result<Fixture, Box<dyn std::error::Error>> {
    let (pool, database_url) =
        common::test_pool_with_url("CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL").await?;
    let workspace_id = WorkspaceId::new();
    sqlx::query("INSERT INTO workspaces (id, slug, name) VALUES ($1, $2, $3)")
        .bind(workspace_id.into_uuid())
        .bind(format!("{label}-{}", workspace_id.into_uuid().simple()))
        .bind("Supply Test Act")
        .execute(&pool)
        .await?;
    let repository = PostgresAutopilotRepository::new(
        pool.clone(),
        &DatabaseConfig {
            url: database_url,
            max_connections: 4,
            connect_timeout: Duration::from_secs(3),
            ping_timeout: Duration::from_secs(2),
            operation_timeout: Duration::from_secs(10),
            lock_timeout: Duration::from_secs(1),
        },
    );
    // A composed letter needs the tenant redirect surface. Release URLs
    // remain external destinations; readers receive ledger-backed CTAs.
    sqlx::query(
        "INSERT INTO tenant_settings (workspace_id, key, value)
         VALUES ($1, 'member_site_base_url', 'https://band.example')",
    )
    .bind(workspace_id.into_uuid())
    .execute(&pool)
    .await?;
    Ok(Fixture {
        pool,
        repository,
        workspace_id,
        now: OffsetDateTime::now_utc(),
    })
}

impl Fixture {
    pub(crate) fn ws(&self) -> Uuid {
        self.workspace_id.into_uuid()
    }

    async fn target(
        &self,
        name: &str,
        kind: &str,
        verified: bool,
        do_not_contact: bool,
        last_reply: Option<&str>,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        self.target_at(
            name,
            kind,
            verified,
            do_not_contact,
            last_reply,
            "test.example",
        )
        .await
    }

    /// A contact whose address is under `domain` — the one fact the registry
    /// holds about where a contact is, and what a show's reach is read from.
    pub(crate) async fn target_at(
        &self,
        name: &str,
        kind: &str,
        verified: bool,
        do_not_contact: bool,
        last_reply: Option<&str>,
        domain: &str,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO outreach_targets
                 (id, workspace_id, target_kind, display_name, contact_email,
                  active, verified, accepts_outreach, do_not_contact, last_reply_disposition)
             VALUES ($1, $2, $3, $4, $5, true, $6, true, $7, COALESCE($8, 'none'))",
        )
        .bind(id)
        .bind(self.ws())
        .bind(kind)
        .bind(name)
        .bind(format!(
            "{}-{}@{domain}",
            name.to_lowercase().replace(' ', "-"),
            id.simple()
        ))
        .bind(verified)
        .bind(do_not_contact)
        .bind(last_reply)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub(crate) async fn release_source(
        &self,
        title: &str,
        url: &str,
        release_type: &str,
        released_at: Option<i64>,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO content_sources
                 (id, workspace_id, source_kind, source_key, title, occurred_at, expires_at, metadata)
             VALUES ($1, $2, 'release', $3, $4, $5, $5 + interval '90 days', $6)",
        )
        .bind(id)
        .bind(self.ws())
        .bind(format!("release:{id}"))
        .bind(title)
        .bind(self.now)
        .bind(serde_json::json!({
            "url": url,
            "release_type": release_type,
            "released_at": released_at.map(|at| at.to_string()),
        }))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// A show in a Polish city: its opportunities reach `.pl` contacts only.
    async fn event(&self, days_out: i64, status: &str) -> Result<Uuid, Box<dyn std::error::Error>> {
        self.event_in_city(days_out, status, "Test City").await
    }

    /// A published show with a city on record — the fact an organiser's
    /// letter cites, and the row the old sender identity mistook for the
    /// act's home.
    pub(crate) async fn event_in_city(
        &self,
        days_out: i64,
        status: &str,
        city_name: &str,
    ) -> Result<Uuid, Box<dyn std::error::Error>> {
        let city = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO cities (id, slug, name, country_code)
             VALUES ($1, $2, $3, 'PL')",
        )
        .bind(city)
        .bind(format!("city-{}", city.simple()))
        .bind(city_name)
        .execute(&self.pool)
        .await?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, slug, title, starts_at, status, published_at, city_id)
             VALUES ($1, $2, $3, 'A show', $4, $5, CASE WHEN $5 = 'published' THEN now() END, $6)",
        )
        .bind(id)
        .bind(self.ws())
        .bind(format!("show-{}", id.simple()))
        .bind(self.now + time::Duration::days(days_out))
        .bind(status)
        .bind(city)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    async fn set_setting(&self, key: &str, value: &str) -> Result<(), Box<dyn std::error::Error>> {
        sqlx::query(
            "INSERT INTO tenant_settings (workspace_id, key, value) VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(self.ws())
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One message on a thread — `None` opportunity is the imported,
    /// hand-sent kind; `Some` is a send the engine itself wrote.
    async fn message(
        &self,
        target: Uuid,
        direction: &str,
        opportunity: Option<Uuid>,
        days_ago: i64,
        source_key: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        sqlx::query(
            "INSERT INTO outreach_interactions
                 (workspace_id, target_id, opportunity_id, direction, phase, source_key, occurred_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(self.ws())
        .bind(target)
        .bind(opportunity)
        .bind(direction)
        .bind(if direction == "inbound" {
            "reply"
        } else {
            "initial"
        })
        .bind(source_key)
        .bind(self.now - time::Duration::days(days_ago))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Active opportunities as (source, subject_key, target name), sorted.
    async fn live(&self) -> Result<Vec<(String, String, String)>, Box<dyn std::error::Error>> {
        Ok(sqlx::query_as::<_, (String, String, String)>(
            "SELECT opportunity.source, opportunity.subject_key, target.display_name
             FROM outreach_opportunities AS opportunity
             JOIN outreach_targets AS target ON target.id = opportunity.target_id
             WHERE opportunity.workspace_id = $1 AND opportunity.active
             ORDER BY 1, 2, 3",
        )
        .bind(self.ws())
        .fetch_all(&self.pool)
        .await?)
    }
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_refresh_keeps_the_pitch_and_the_next_shows_supplied()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-refresh").await?;
    // The catalogue: an older album, a newer single, and a synced album with
    // no release date — its sync time must not make it the newest.
    f.release_source(
        "Old Album",
        "https://listen.example/old",
        "album",
        Some(1_700_000_000),
    )
    .await?;
    let single = f
        .release_source(
            "New Single",
            "https://listen.example/new",
            "single",
            Some(1_750_000_000),
        )
        .await?;
    f.release_source("Undated Sync", "https://listen.example/sync", "album", None)
        .await?;

    f.target("Zine", "press", true, false, None).await?;
    f.target("Station", "radio", true, false, None).await?;
    // The only contact in the show's country: the show is news to it alone.
    f.target_at("Local Zine", "press", true, false, None, "zine.example.pl")
        .await?;
    f.target("Answered Blog", "press", true, false, Some("received"))
        .await?;
    f.target("Unverified Mag", "press", false, false, None)
        .await?;
    f.target("Silent Please", "press", true, true, None).await?;
    f.target("A Playlist", "playlist", true, false, None)
        .await?;

    let soon = f.event(20, "published").await?;
    f.event(90, "published").await?; // Too far out to pitch yet.
    f.event(2, "published").await?; // Too close to pitch at all.
    f.event(20, "cancelled").await?;

    let report = f
        .repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(report.pitch.as_deref(), Some("New Single"));
    let catalogue_key = format!("catalogue:{single}");
    let show_key = format!("event:{soon}");
    assert_eq!(
        f.live().await?,
        vec![
            (
                "catalogue_autopilot".to_owned(),
                catalogue_key.clone(),
                "Local Zine".to_owned()
            ),
            (
                "catalogue_autopilot".to_owned(),
                catalogue_key.clone(),
                "Station".to_owned()
            ),
            (
                "catalogue_autopilot".to_owned(),
                catalogue_key.clone(),
                "Zine".to_owned()
            ),
            // The show reaches the contact in its own country and no one
            // else: a `test.example` address says nothing about where its
            // owner is.
            (
                "event_autopilot".to_owned(),
                show_key.clone(),
                "Local Zine".to_owned()
            ),
        ]
    );

    // Idempotent: a second cycle changes nothing.
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(f.live().await?.len(), 4);

    // A contact imported after the announcement is supplied next cycle.
    f.target_at("Late Import", "press", true, false, None, "late.example.pl")
        .await?;
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(f.live().await?.len(), 6);

    // An operator's release plan with a listen link becomes the pitch; the
    // catalogue's rows retire. A cancelled show's rows retire too.
    let plan = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO release_plans (id, workspace_id, source_key, title, release_at, listen_url)
         VALUES ($1, $2, $3, 'The Plan', $4, 'https://listen.example/plan')",
    )
    .bind(plan)
    .bind(f.ws())
    .bind(format!("plan-{}", plan.simple()))
    .bind(f.now + time::Duration::days(10))
    .execute(&f.pool)
    .await?;
    sqlx::query("UPDATE events SET status = 'cancelled' WHERE id = $1")
        .bind(soon)
        .execute(&f.pool)
        .await?;
    let report = f
        .repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(report.pitch.as_deref(), Some("The Plan"));
    assert_eq!(
        report.opportunities_retired, 6,
        "four catalogue rows and two show rows"
    );
    let live = f.live().await?;
    assert_eq!(live.len(), 4, "{live:?}");
    assert!(live.iter().all(|(source, key, _)| {
        source == "release_autopilot" && *key == format!("release:{plan}")
    }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_catalogue_is_pitched_in_one_wave_of_composed_letters()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-wave").await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 2}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    // More contacts than one wave holds: the ones past its capacity must wait
    // for next month's wave, not arrive as loose approval cards.
    for index in 0..14 {
        f.target(&format!("Zine {index}"), "press", true, false, None)
            .await?;
    }

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    let report = EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;

    let waves = sqlx::query_as::<_, (String, String, i32)>(
        "SELECT anchor_kind, target_kind, capacity FROM outreach_waves WHERE workspace_id = $1",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?;
    assert!(
        waves
            .iter()
            .any(|(anchor, kind, _)| anchor == "catalogue" && kind == "press"),
        "a catalogue press wave opens: {waves:?} (report: {report:?})"
    );

    let pitches = sqlx::query_as::<_, (Option<String>, String, String)>(
        "SELECT payload->>'wave_id', payload->'draft'->>'body', status
         FROM autopilot_actions
         WHERE workspace_id = $1 AND context = 'outreach'",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?;
    let capacity = waves
        .iter()
        .find(|(anchor, kind, _)| anchor == "catalogue" && kind == "press")
        .map(|(_, _, capacity)| usize::try_from(*capacity).unwrap_or(0))
        .unwrap_or(0);
    assert!((2..14).contains(&capacity), "capacity {capacity}");
    assert_eq!(
        pitches.len(),
        capacity,
        "the wave fills to its capacity and nothing spills over: {pitches:?} (report: {report:?})"
    );
    for (wave_id, body, status) in &pitches {
        assert!(wave_id.is_some(), "a catalogue pitch is never a loose card");
        assert_eq!(status, "awaiting_approval");
        assert!(
            body.contains("Echoes") && body.contains("https://band.example/l/"),
            "the letter names the pitch and links it: {body}"
        );
        let destination_is_live: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM smart_links
                 WHERE workspace_id = $1 AND active
                   AND destination_url = 'https://open.spotify.example/album/echoes'
                   AND strpos($2, 'https://band.example/l/' || slug) > 0
             )",
        )
        .bind(f.ws())
        .bind(body)
        .fetch_one(&f.pool)
        .await?;
        assert!(
            destination_is_live,
            "the printed CTA must resolve to this catalogue"
        );
        assert!(!body.contains("https://open.spotify.example/album/echoes"));

    }
    Ok(())
}

/// The supply refresh re-observes live opportunities every cycle. A
/// re-observation on the same day is the same finding, not a new decision:
/// keyed by the instant, the ledger grew by every candidate every five
/// minutes (18,757 outreach decisions in one day of production).
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_re_observed_opportunity_is_not_a_new_decision_every_cycle()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-ledger").await?;
    // The production shape: the week's asks are spent, so every pitch is a
    // recommendation, no action goes in flight, and the same candidates come
    // back every cycle.
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run, weekly_approval_requests)
         VALUES ($1, true, false, 0)
         ON CONFLICT (workspace_id) DO UPDATE
         SET agent_enabled = true, dry_run = false, weekly_approval_requests = 0",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 2}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    for index in 0..4 {
        f.target_at(
            &format!("Radio {index}"),
            "radio",
            true,
            false,
            None,
            "radio.example.pl",
        )
        .await?;
    }
    // An upcoming show: its pitches are loose, not wave-bound, which is the
    // kind production re-decided every cycle.
    f.event(20, "published").await?;
    let decisions = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM autopilot_decisions
             WHERE workspace_id = $1 AND decision_kind = 'request_relationship_outreach'",
        )
        .bind(f.ws())
        .fetch_one(&f.pool)
        .await
    };

    // Two cycles five minutes apart, each after its own refresh — early in
    // the day, so both fall on the same date.
    let first = f.now.replace_time(time::Time::from_hms(6, 0, 0)?);
    for at in [first, first + time::Duration::minutes(5)] {
        sqlx::query("UPDATE outreach_opportunities SET observed_at = $2 WHERE workspace_id = $1")
            .bind(f.ws())
            .bind(at)
            .execute(&f.pool)
            .await?;
        f.repository
            .refresh_outreach_supply(f.workspace_id, at)
            .await?;
        EvaluateAutopilot::new(&f.repository, f.workspace_id)
            .execute(at)
            .await?;
    }
    let after_two = decisions().await?;
    assert!(after_two > 0, "the cycle decided something");

    let third = first + time::Duration::minutes(10);
    sqlx::query("UPDATE outreach_opportunities SET observed_at = $2 WHERE workspace_id = $1")
        .bind(f.ws())
        .bind(third)
        .execute(&f.pool)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(third)
        .await?;
    assert_eq!(
        decisions().await?,
        after_two,
        "re-observing the same opportunities later the same day writes no new decisions"
    );
    Ok(())
}

/// Half of the act's pitchable contacts have a `.pl` address. Their letter is
/// written in Polish; everyone else's stays English.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_polish_address_gets_a_polish_letter() -> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-polish").await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 2}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    f.release_source(
        "Echoes",
        "https://listen.example/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    let polish = f.target("Polski Zin", "press", true, false, None).await?;
    sqlx::query("UPDATE outreach_targets SET contact_email = $2 WHERE id = $1")
        .bind(polish)
        .bind(format!("redakcja-{}@zin.pl", polish.simple()))
        .execute(&f.pool)
        .await?;
    f.target("English Zine", "press", true, false, None).await?;

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;

    let letters = sqlx::query_as::<_, (String, String)>(
        "SELECT payload->>'target_id', payload->'draft'->>'body'
         FROM autopilot_actions WHERE workspace_id = $1 AND context = 'outreach'",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(letters.len(), 2, "{letters:?}");
    for (target, body) in &letters {
        if *target == polish.to_string() {
            assert!(body.starts_with("Dzień dobry, Polski Zin,\n"), "{body}");
            assert!(body.contains("Pozdrawiamy,"), "{body}");
        } else {
            assert!(body.starts_with("Hi English Zine,"), "{body}");
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_hand_written_threads_seed_one_follow_up_each() -> Result<(), Box<dyn std::error::Error>>
{
    let f = fixture("supply-threads").await?;

    // The thread the act started by hand: one unlinked outbound inside the
    // window is exactly the follow-up opportunity.
    let warm = f
        .target("Bydgoszcz Radio", "press", true, false, None)
        .await?;
    f.message(warm, "outbound", None, 30, "master:ORC-1")
        .await?;
    // Their answer is the last word — the inbound kills the seed even before
    // the reply is classified onto the target.
    let answered = f.target("Replied Zine", "press", true, false, None).await?;
    f.message(answered, "outbound", None, 30, "master:ORC-2")
        .await?;
    f.message(answered, "inbound", None, 5, "master:ORC-3")
        .await?;
    // Inside the window but not yet due — the row seeds now and the
    // evaluator holds it until the thread turns ten days old.
    let fresh = f
        .target("Fresh Contact", "press", true, false, None)
        .await?;
    f.message(fresh, "outbound", None, 3, "master:ORC-4")
        .await?;
    // Older than the window — the thread aged out.
    let stale = f.target("Aged Out", "press", true, false, None).await?;
    f.message(stale, "outbound", None, 70, "master:ORC-5")
        .await?;
    // Suppressed and representation-kind contacts never seed.
    let quiet = f.target("Quiet Please", "press", true, true, None).await?;
    f.message(quiet, "outbound", None, 30, "master:ORC-6")
        .await?;
    // Representation kinds need a stated basis to stay writable — set it at
    // insert so it is the kind filter, not the acceptance check, that
    // excludes the contact.
    let agent = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO outreach_targets
             (id, workspace_id, target_kind, display_name, contact_email,
              active, verified, accepts_outreach, accepts_outreach_basis, do_not_contact)
         VALUES ($1, $2, 'agent', 'An Agency', $3, true, true, true, 'met at showcase', false)",
    )
    .bind(agent)
    .bind(f.ws())
    .bind(format!("an-agency-{}@test.example", agent.simple()))
    .execute(&f.pool)
    .await?;
    f.message(agent, "outbound", None, 30, "master:ORC-7")
        .await?;

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    let mut live = f.live().await?;
    live.sort();
    let mut expected = vec![
        (
            "thread_followup".to_owned(),
            format!("thread:{fresh}"),
            "Fresh Contact".to_owned(),
        ),
        (
            "thread_followup".to_owned(),
            format!("thread:{warm}"),
            "Bydgoszcz Radio".to_owned(),
        ),
    ];
    expected.sort();
    assert_eq!(live, expected);

    // Idempotent — a second cycle changes nothing.
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(f.live().await?.len(), 2);

    // Their answer retires the thread: the inbound is the last word.
    f.message(warm, "inbound", None, 1, "master:ORC-8").await?;
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert_eq!(f.live().await?.len(), 1);

    // The follow-up itself going out retires it too — the newest word is
    // then a linked outbound, so no unlinked last word remains to chase.
    let (opportunity,): (Uuid,) = sqlx::query_as(
        "SELECT id FROM outreach_opportunities
         WHERE workspace_id = $1 AND target_id = $2",
    )
    .bind(f.ws())
    .bind(fresh)
    .fetch_one(&f.pool)
    .await?;
    f.message(fresh, "outbound", Some(opportunity), 0, "autopilot:send")
        .await?;
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert!(f.live().await?.is_empty());
    Ok(())
}

/// An organiser is pitched a slot, not a review — the letter asks to play
/// and cites the act's next confirmed show, which the same show used to
/// supply the "from" line instead.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_organiser_gets_a_gig_request_citing_the_next_show()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-organiser").await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 2}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    // The band declares home — the upcoming show's city must not become it.
    f.set_setting("act_home_city", "Wrocław").await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    f.event_in_city(20, "published", "Gorzów Wielkopolski")
        .await?;
    let organiser = f
        .target(
            "uROCK Młodych — organizator",
            "organiser",
            true,
            false,
            None,
        )
        .await?;
    // A press contact alongside, to prove only the organiser's letter changes.
    f.target("Metal Noise", "press", true, false, None).await?;

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    assert!(
        f.live()
            .await?
            .iter()
            .any(|(source, _, name)| source == "event_autopilot" && name.starts_with("uROCK")),
        "the show seeds the organiser an opportunity"
    );
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;

    let (key, subject, body, status): (String, String, String, String) = sqlx::query_as(
        "SELECT payload->>'template_key', payload->'draft'->>'subject',
                payload->'draft'->>'body', status
         FROM autopilot_actions
         WHERE workspace_id = $1 AND context = 'outreach'
           AND payload->>'target_id' = $2",
    )
    .bind(f.ws())
    .bind(organiser.to_string())
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(key, "outreach.organiser.v1", "{key}");
    assert_eq!(status, "awaiting_approval", "{status}");
    assert!(
        subject.contains("gig request"),
        "the subject names the ask: {subject}"
    );
    // The ask is a slot, the proof is the calendar — the show city appears
    // inside the citation and never as the band's home.
    assert!(body.contains("a slot on your bill"), "{body}");
    assert!(body.contains("Gorzów Wielkopolski"), "{body}");
    assert!(body.contains("from Wrocław"), "{body}");
    assert!(!body.contains("from Gorzów"), "{body}");
    let lower = body.to_lowercase();
    assert!(!lower.contains("review"), "{body}");
    assert!(!lower.contains("submit"), "{body}");

    // A draft composed before the home city was declared — or by the old
    // most-played-city guess — still waits in the queue naming the show's
    // city as home. The re-compose the cycle runs writes the declared one.
    sqlx::query(
        "UPDATE autopilot_actions
         SET payload = jsonb_set(payload, '{draft,body}', to_jsonb(replace(payload->'draft'->>'body',
                 'from Wrocław', 'from Gorzów Wielkopolski')))
         WHERE workspace_id = $1 AND payload->>'target_id' = $2",
    )
    .bind(f.ws())
    .bind(organiser.to_string())
    .execute(&f.pool)
    .await?;
    let changed = f
        .repository
        .recompose_pending_letters(f.workspace_id)
        .await?;
    assert!(changed >= 1, "the stale letter is re-composed");
    let body: String = sqlx::query_scalar(
        "SELECT payload->'draft'->>'body' FROM autopilot_actions
         WHERE workspace_id = $1 AND payload->>'target_id' = $2",
    )
    .bind(f.ws())
    .bind(organiser.to_string())
    .fetch_one(&f.pool)
    .await?;
    assert!(body.contains("from Wrocław"), "{body}");
    assert!(!body.contains("from Gorzów"), "{body}");
    assert_eq!(
        f.repository
            .recompose_pending_letters(f.workspace_id)
            .await?,
        0,
        "a letter already current is left alone"
    );

    // The band fixes a sentence without approving — the pitch sits in a wave
    // and cannot be approved on its own. The edit is saved, recorded, and the
    // next re-compose leaves the band's words alone.
    let action_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM autopilot_actions WHERE workspace_id = $1 AND payload->>'target_id' = $2",
    )
    .bind(f.ws())
    .bind(organiser.to_string())
    .fetch_one(&f.pool)
    .await?;
    let edited = body.replace("from Wrocław", "from Wrocław, Poland");
    let mut revision = std::collections::BTreeMap::new();
    revision.insert("body".to_owned(), edited.clone());
    let revisable = f
        .repository
        .revise_pending_draft(
            f.workspace_id,
            action_id,
            &revision,
            &crowdrelay_application::IdempotencyKey::parse(format!("revise-{action_id}"))?,
            None,
        )
        .await?;
    assert_eq!(revisable.get("body"), Some(&edited));
    let recorded: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM draft_revisions WHERE workspace_id = $1 AND action_id = $2",
    )
    .bind(f.ws())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(recorded, 1, "the edit is recorded as a revision");
    f.repository
        .recompose_pending_letters(f.workspace_id)
        .await?;
    let kept: String = sqlx::query_scalar(
        "SELECT payload->'draft'->>'body' FROM autopilot_actions WHERE workspace_id = $1 AND id = $2",
    )
    .bind(f.ws())
    .bind(action_id)
    .fetch_one(&f.pool)
    .await?;
    assert_eq!(kept, edited, "a draft the band edited is never re-composed");
    Ok(())
}

/// The sender's city is a declaration, not a measurement: with the setting
/// absent the letter carries no city at all — the upcoming show's city used
/// to silently fill it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_home_city_is_declared_never_measured() -> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-home-city").await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 1}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    // The catalogue letter is the one under test: a show letter names the
    // show's city by design, and the `test.example` contacts here are not in
    // the show's country, so the show writes them nothing.
    // The only city in the workspace's calendar — the old heuristic would
    // have announced the act as being from here.
    f.event_in_city(20, "published", "Gorzów Wielkopolski")
        .await?;
    f.target("Metal Noise", "press", true, false, None).await?;

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;

    let (body,): (String,) = sqlx::query_as(
        "SELECT payload->'draft'->>'body' FROM autopilot_actions
         WHERE workspace_id = $1 AND context = 'outreach'",
    )
    .bind(f.ws())
    .fetch_one(&f.pool)
    .await?;
    assert!(
        body.contains("I am writing from Supply Test Act,"),
        "{body}"
    );
    assert!(!body.contains("Gorzów"), "{body}");

    // Declared, the same event sits beside a letter that says Wrocław.
    f.set_setting("act_home_city", "Wrocław").await?;
    f.target("Second Zine", "press", true, false, None).await?;
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;
    let bodies: Vec<String> = sqlx::query_scalar(
        "SELECT payload->'draft'->>'body' FROM autopilot_actions
         WHERE workspace_id = $1 AND context = 'outreach'",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?;
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(
        bodies.iter().any(|body| body.contains("from Wrocław")),
        "the declared city lands: {bodies:?}"
    );
    assert!(
        bodies.iter().all(|body| !body.contains("from Gorzów")),
        "the show's city is never the act's: {bodies:?}"
    );
    Ok(())
}

/// A show opportunity writes the show letter — city, date, room, tickets —
/// and only to contacts in the show's country. Before 2026-09-27 it wrote the
/// catalogue pitch ("we would love to submit {album} for coverage") to every
/// press contact anywhere, and thirty-four such letters about one Gorzów show
/// sat in the approval queue.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_show_opportunity_writes_the_show_letter_to_local_contacts_only()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("supply-show-letter").await?;
    sqlx::query(
        "INSERT INTO growth_envelope (workspace_id, agent_enabled, dry_run) VALUES ($1, true, false)
         ON CONFLICT (workspace_id) DO UPDATE SET agent_enabled = true, dry_run = false",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    sqlx::query(
        "INSERT INTO autopilot_policies
             (workspace_id, context, enabled, autonomy_level, minimum_confidence_basis_points,
              max_actions_24h, config)
         VALUES ($1, 'outreach', true, 'require_approval', 7500, 20,
                 '{\"waves\": {\"min_pitches_per_wave\": 2}}')
         ON CONFLICT (workspace_id, context) DO UPDATE SET enabled = true,
             autonomy_level = 'require_approval', minimum_confidence_basis_points = 7500,
             max_actions_24h = 20, config = EXCLUDED.config",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    f.release_source(
        "Echoes",
        "https://open.spotify.example/album/echoes",
        "album",
        Some(1_746_057_600),
    )
    .await?;
    let local = f
        .target_at(
            "Radio Lokalne",
            "radio",
            true,
            false,
            None,
            "radio.example.pl",
        )
        .await?;
    let abroad = f
        .target_at(
            "Festival Abroad",
            "radio",
            true,
            false,
            None,
            "festival.example.sk",
        )
        .await?;
    let show = f
        .event_in_city(20, "published", "Gorzów Wielkopolski")
        .await?;
    sqlx::query("UPDATE events SET venue = 'MagnetOffOn', ticket_url = 'https://tickets.example/gorzow' WHERE id = $1")
        .bind(show)
        .execute(&f.pool)
        .await?;

    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;

    let letters = sqlx::query_as::<_, (Uuid, String, String, String)>(
        "SELECT (action.payload->>'target_id')::uuid, action.payload->>'template_key',
                action.payload->'draft'->>'subject', action.payload->'draft'->>'body'
         FROM autopilot_actions AS action
         JOIN outreach_opportunities AS opportunity
           ON opportunity.id = (action.payload->>'opportunity_id')::uuid
         WHERE action.workspace_id = $1 AND action.context = 'outreach'
           AND opportunity.source = 'event_autopilot'",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?;
    assert!(
        letters.iter().all(|(target, ..)| *target != abroad),
        "a show is not news to a contact in another country: {letters:?}"
    );
    let (_, template_key, subject, body) = letters
        .iter()
        .find(|(target, ..)| *target == local)
        .expect("the local contact gets a show letter");
    assert_eq!(template_key, "event.press.v1");
    assert!(subject.contains("Gorzów Wielkopolski"), "{subject}");
    assert!(
        body.contains("gramy koncert: Gorzów Wielkopolski, MagnetOffOn."),
        "{body}"
    );
    assert!(
        body.contains("Bilety: https://band.example/l/"),
        "{body}"
    );
    let ticket_is_live: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM smart_links
             WHERE workspace_id = $1 AND active
               AND destination_url = 'https://tickets.example/gorzow'
               AND strpos($2, 'https://band.example/l/' || slug) > 0
         )",
    )
    .bind(f.ws())
    .bind(body)
    .fetch_one(&f.pool)
    .await?;
    assert!(
        ticket_is_live,
        "the show CTA must resolve to its real ticket destination"
    );
    assert!(!body.contains("https://tickets.example/gorzow"));

    assert!(
        !body.contains("zaproponować Wam Echoes"),
        "the album pitch: {body}"
    );
    Ok(())
}

/// A "no, thanks" starts its cooldown on the day it landed, and the evaluator
/// can only count from it when the snapshot carries the decline's own
/// timestamp — this is the query that teaches it.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_declined_reply_starts_its_cooldown_on_the_day_it_landed()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("decline-clock").await?;
    let target = f.target("Grumpy Zine", "press", true, false, None).await?;
    sqlx::query(
        "INSERT INTO outreach_opportunities
             (workspace_id, target_id, source, subject_kind, subject_key, template_key,
              relevance_basis_points, confidence_basis_points, observed_at, expires_at)
         VALUES ($1, $2, 'release_autopilot', 'release', 'release:x', 'release',
                 8500, 9000, $3, $4)",
    )
    .bind(f.ws())
    .bind(target)
    .bind(f.now - time::Duration::days(400))
    .bind(f.now + time::Duration::days(30))
    .execute(&f.pool)
    .await?;
    // The pitch is ancient; the "no" is fresh. The clock belongs to the "no".
    f.message(target, "outbound", None, 400, "pitch-1").await?;
    sqlx::query(
        "INSERT INTO outreach_interactions
             (workspace_id, target_id, opportunity_id, direction, phase, disposition,
              source_key, occurred_at)
         VALUES ($1, $2, NULL, 'inbound', 'reply', 'declined', 'decline-1', $3)",
    )
    .bind(f.ws())
    .bind(target)
    .bind(f.now - time::Duration::days(30))
    .execute(&f.pool)
    .await?;

    let snapshots = f
        .repository
        .load_target_outreach_snapshots(f.workspace_id, OutreachTargetId::from_uuid(target))
        .await?;
    assert_eq!(snapshots.len(), 1);
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.last_reply, OutreachReplyDisposition::Declined);
    let declined_at = snapshot
        .last_declined_at
        .expect("the decline's own timestamp is in the snapshot");
    assert!(
        (declined_at - (f.now - time::Duration::days(30))).abs() < time::Duration::seconds(1),
        "the decline time survived the round trip: {declined_at}"
    );
    Ok(())
}
