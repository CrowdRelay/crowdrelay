//! Nobody is pitched unread — the outreach engine's half of the rule.
//!
//! The evaluator holds a target with no recent, sourced fact on file
//! (`NeedsResearch`), so no pitch is even proposed for them; once the band has
//! read them the pitch is proposed, and the letter opens with what was read,
//! before anything the band wants. The fixture contacts of the other engine
//! proofs are all pre-read; here one is not.

use crate::autopilot_outreach_engine::fixture;

use crowdrelay_application::autopilot::EvaluateAutopilot;

async fn arm(
    f: &crate::autopilot_outreach_engine::Fixture,
) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}

async fn cycle(
    f: &crate::autopilot_outreach_engine::Fixture,
) -> Result<(), Box<dyn std::error::Error>> {
    f.repository
        .refresh_outreach_supply(f.workspace_id, f.now)
        .await?;
    EvaluateAutopilot::new(&f.repository, f.workspace_id)
        .execute(f.now)
        .await?;
    Ok(())
}

async fn letters(
    f: &crate::autopilot_outreach_engine::Fixture,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as::<_, (String, String)>(
        "SELECT payload->>'target_id', payload->'draft'->>'body'
         FROM autopilot_actions WHERE workspace_id = $1 AND context = 'outreach'",
    )
    .bind(f.ws())
    .fetch_all(&f.pool)
    .await?)
}

#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn an_unread_target_is_not_pitched_until_the_band_has_read_them()
-> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("research-gate").await?;
    arm(&f).await?;
    // Two press targets. Both are verified, open and fresh; one has been read.
    let read = f
        .target_at("Czytany Zin", "press", true, false, None, "zin.pl")
        .await?;
    let unread = f
        .target_at("Nieczytany Zin", "press", true, false, None, "zin.pl")
        .await?;
    sqlx::query(
        "DELETE FROM contact_research WHERE workspace_id = $1
           AND normalized_email = (SELECT lower(contact_email) FROM outreach_targets WHERE id = $2)",
    )
    .bind(f.ws())
    .bind(unread)
    .execute(&f.pool)
    .await?;

    cycle(&f).await?;
    let first = letters(&f).await?;
    assert!(
        first
            .iter()
            .all(|(target, _)| *target != unread.to_string()),
        "an unread target was pitched: {first:?}"
    );
    assert!(
        first.iter().any(|(target, _)| *target == read.to_string()),
        "the read target should still be pitched: {first:?}"
    );

    // The band reads them: the next cycle proposes the pitch.
    let email: String =
        sqlx::query_scalar("SELECT contact_email FROM outreach_targets WHERE id = $1")
            .bind(unread)
            .fetch_one(&f.pool)
            .await?;
    f.read(&email).await?;
    cycle(&f).await?;
    let second = letters(&f).await?;
    let (_, body) = second
        .iter()
        .find(|(target, _)| *target == unread.to_string())
        .unwrap_or_else(|| panic!("a read target was still not pitched: {second:?}"));

    // The letter opens with what was read, between the greeting and the ask,
    // and never prints where it was found.
    let greeting = body.find("Dzień dobry, Nieczytany Zin,").expect("greeting");
    let known = body
        .find("recenzja płyty „Szum” w audycji „Metalowy Wieczór”")
        .expect("the fact");
    let ask = body.find("Echoes").expect("the pitch");
    assert!(greeting < known && known < ask, "{body}");
    assert!(
        body.contains("W recenzji „Szum” zwróciło nam uwagę"),
        "the pitch must open on the sourced human observation: {body}"
    );
    assert!(!body.contains("Zanim napisaliśmy"), "{body}");
    assert!(
        !body.contains("example.test/"),
        "the research source leaked into the letter: {body}"
    );
    Ok(())
}

/// A fact ages out: a target read long ago is unread again, and the engine says
/// so rather than pitching on a stale compliment.
#[tokio::test]
#[ignore = "requires CROWDRELAY_AUTOPILOT_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn a_stale_fact_does_not_open_the_door() -> Result<(), Box<dyn std::error::Error>> {
    let f = fixture("research-stale").await?;
    arm(&f).await?;
    let target = f
        .target_at("Stary Zin", "press", true, false, None, "zin.pl")
        .await?;
    sqlx::query(
        "UPDATE contact_research SET observed_on = (now() AT TIME ZONE 'UTC')::date - 200
         WHERE workspace_id = $1",
    )
    .bind(f.ws())
    .execute(&f.pool)
    .await?;
    cycle(&f).await?;
    let all = letters(&f).await?;
    assert!(
        all.iter().all(|(t, _)| *t != target.to_string()),
        "a 200-day-old fact opened the door: {all:?}"
    );
    Ok(())
}
