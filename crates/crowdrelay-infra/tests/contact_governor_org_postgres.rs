//! The contact governor must bind across an organization, not just a workspace.
//!
//! `viryaos_contact_governor` is keyed `(workspace_id, normalized_contact)`, and
//! an act is a workspace: a roster of eight acts is eight workspaces under one
//! `organizations` row. Keyed that way alone, the same person takes one message
//! per act per week with every cooldown satisfied, and a `do_not_contact` given
//! to one act does not bind the others. To someone who reads the roster as one
//! sender, that is not a cooldown at all — and it is true today for any
//! organization holding two workspaces.
//!
//! `reserve_contact_window` now gates its insert on no sibling workspace in the
//! same organization holding a live block. It stays one statement, because a
//! pre-check followed by an insert leaves a gap two acts could both pass
//! through.
//!
//! # Why this file re-states the predicate
//!
//! Every function in `autopilot::execution_capabilities` is
//! `pub(in crate::autopilot)`, so an integration test cannot call the
//! reservation, and widening one function's visibility to suit a test would
//! break the module's own convention. So the gate is exercised here as the same
//! `NOT EXISTS` against a real schema — which is the part that can actually be
//! wrong: the organization match, the self-exclusion, and the
//! do-not-contact-or-unexpired disjunction.
//!
//! A copied predicate can drift from the original, so the last test reads the
//! production source and fails if the gate is no longer there. That does not
//! prove the two are identical; it does mean deleting the gate cannot pass
//! silently.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

const TEST_DATABASE_URL_KEY: &str = "CROWDRELAY_TEST_DATABASE_URL";

/// The sibling-block gate, verbatim from `reserve_contact_window`'s insert.
const SIBLING_BLOCK: &str = r#"
SELECT NOT EXISTS (
    SELECT 1
    FROM viryaos_contact_governor sibling
    JOIN workspaces sibling_ws ON sibling_ws.id = sibling.workspace_id
    JOIN workspaces self_ws ON self_ws.id = $1
    WHERE sibling.normalized_contact = $2
      AND sibling.workspace_id <> $1
      AND self_ws.organization_id IS NOT NULL
      AND sibling_ws.organization_id = self_ws.organization_id
      AND (sibling.do_not_contact OR sibling.next_contact_after > $3)
)
"#;

async fn pool() -> PgPool {
    let url = std::env::var(TEST_DATABASE_URL_KEY)
        .expect("set CROWDRELAY_TEST_DATABASE_URL to a disposable database");
    PgPool::connect(&url)
        .await
        .expect("connect to test database")
}

async fn cleanup(pool: &PgPool, workspaces: &[Uuid], organizations: &[Uuid]) {
    sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
        .bind(workspaces)
        .execute(pool)
        .await
        .expect("cascade workspaces");
    sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
        .bind(organizations)
        .execute(pool)
        .await
        .expect("delete organizations");
}

async fn seed_organization(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, slug, name) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(format!("org-{}", id.simple()))
        .bind("Test roster")
        .execute(pool)
        .await
        .expect("seed organization");
    id
}

async fn seed_act(pool: &PgPool, organization: Option<Uuid>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id, slug, name, organization_id) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(format!("act-{}", id.simple()))
        .bind("Test act")
        .bind(organization)
        .execute(pool)
        .await
        .expect("seed workspace");
    id
}

/// Writes a governor row for `workspace`, as a real send would.
async fn record_contact(
    pool: &PgPool,
    workspace: Uuid,
    contact: &str,
    next_contact_after: OffsetDateTime,
    do_not_contact: bool,
) {
    sqlx::query(
        r#"
        INSERT INTO viryaos_contact_governor (
            workspace_id, normalized_contact, last_context,
            last_outbound_at, next_contact_after, do_not_contact
        ) VALUES ($1, $2, 'test', $3, $4, $5)
        "#,
    )
    .bind(workspace)
    .bind(contact)
    .bind(next_contact_after - time::Duration::days(7))
    .bind(next_contact_after)
    .bind(do_not_contact)
    .execute(pool)
    .await
    .expect("record contact");
}

async fn may_reserve(pool: &PgPool, workspace: Uuid, contact: &str, now: OffsetDateTime) -> bool {
    sqlx::query_scalar::<_, bool>(SIBLING_BLOCK)
        .bind(workspace)
        .bind(contact)
        .bind(now)
        .fetch_one(pool)
        .await
        .expect("evaluate sibling block")
}

#[tokio::test]
async fn a_sibling_acts_cooldown_binds_the_whole_roster() {
    let pool = pool().await;
    let organization = seed_organization(&pool).await;
    let act_a = seed_act(&pool, Some(organization)).await;
    let act_b = seed_act(&pool, Some(organization)).await;
    let now = OffsetDateTime::now_utc();
    let fan = format!("fan-{}@example.test", Uuid::now_v7().simple());

    // Act A messaged the fan today; its window runs for seven days.
    record_contact(&pool, act_a, &fan, now + time::Duration::days(7), false).await;

    assert!(
        !may_reserve(&pool, act_b, &fan, now).await,
        "act B may message a fan act A reached today — one person, two bands, one week"
    );

    // Once A's window lapses, B is free. The rule is a cooldown, not a lock.
    assert!(
        may_reserve(&pool, act_b, &fan, now + time::Duration::days(8)).await,
        "act B is still blocked after act A's window expired"
    );

    cleanup(&pool, &[act_a, act_b], &[organization]).await;
}

#[tokio::test]
async fn a_do_not_contact_on_one_act_binds_every_act() {
    let pool = pool().await;
    let organization = seed_organization(&pool).await;
    let act_a = seed_act(&pool, Some(organization)).await;
    let act_b = seed_act(&pool, Some(organization)).await;
    let now = OffsetDateTime::now_utc();
    let fan = format!("fan-{}@example.test", Uuid::now_v7().simple());

    // The fan opted out of act A a year ago — the window is long expired, so
    // only `do_not_contact` can carry the refusal.
    record_contact(&pool, act_a, &fan, now - time::Duration::days(365), true).await;

    assert!(
        !may_reserve(&pool, act_b, &fan, now).await,
        "a fan who opted out of one act still hears from its label-mate"
    );

    cleanup(&pool, &[act_a, act_b], &[organization]).await;
}

#[tokio::test]
async fn an_unrelated_workspace_is_not_bound() {
    // Two tenants who share nothing must not leak a contact decision to each
    // other: that would be one tenant learning another's suppression list.
    let pool = pool().await;
    let organization = seed_organization(&pool).await;
    let act = seed_act(&pool, Some(organization)).await;
    let stranger = seed_act(&pool, None).await;
    let now = OffsetDateTime::now_utc();
    let fan = format!("fan-{}@example.test", Uuid::now_v7().simple());

    record_contact(&pool, act, &fan, now + time::Duration::days(7), true).await;

    assert!(
        may_reserve(&pool, stranger, &fan, now).await,
        "an unrelated tenant was blocked by another tenant's governor row"
    );

    cleanup(&pool, &[act, stranger], &[organization]).await;
}

#[tokio::test]
async fn a_lone_tenant_reserves_exactly_as_before() {
    // Every tenant today has no organization. The gate must be vacuously true
    // for them, or this change alters live behaviour for the only customer.
    let pool = pool().await;
    let solo = seed_act(&pool, None).await;
    let other = seed_act(&pool, None).await;
    let now = OffsetDateTime::now_utc();
    let fan = format!("fan-{}@example.test", Uuid::now_v7().simple());

    record_contact(&pool, other, &fan, now + time::Duration::days(7), true).await;

    assert!(
        may_reserve(&pool, solo, &fan, now).await,
        "a workspace with no organization was gated by a row it cannot be related to"
    );

    cleanup(&pool, &[solo, other], &[]).await;
}

#[tokio::test]
async fn an_act_does_not_block_itself() {
    // The sibling clause excludes the reserving workspace, because the row it
    // is about to update is its own — the per-workspace rules in the
    // ON CONFLICT arm decide that case, and always did.
    let pool = pool().await;
    let organization = seed_organization(&pool).await;
    let act = seed_act(&pool, Some(organization)).await;
    let now = OffsetDateTime::now_utc();
    let fan = format!("fan-{}@example.test", Uuid::now_v7().simple());

    record_contact(&pool, act, &fan, now + time::Duration::days(7), true).await;

    assert!(
        may_reserve(&pool, act, &fan, now).await,
        "the sibling gate fired on the reserving workspace's own row"
    );

    cleanup(&pool, &[act], &[organization]).await;
}

/// The predicate above is a copy; this is what stops the copy outliving the
/// original. It does not prove the two match — it proves the gate still exists.
#[test]
fn the_reservation_still_carries_the_sibling_gate() {
    let source = include_str!("../src/autopilot/execution_capabilities.rs");
    for fragment in [
        "FROM viryaos_contact_governor sibling",
        "self_ws.organization_id IS NOT NULL",
        "sibling_ws.organization_id = self_ws.organization_id",
        "sibling.do_not_contact OR sibling.next_contact_after",
    ] {
        assert!(
            source.contains(fragment),
            "reserve_contact_window no longer gates on sibling workspaces: {fragment:?} is gone"
        );
    }
}
