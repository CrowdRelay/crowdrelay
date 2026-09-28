//! A community target's status follows its latest screening verdict.
//!
//! Both target writers call `community_target_status` (migration 0375). Before
//! it, a community admitted once and refused later stayed `promoted` with a
//! `refused` verdict (r/Metal and r/doommetal in production), and a refused
//! community re-screened as admitted stayed `proposed`.

use crate::common;

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and a disposable PostgreSQL database"]
async fn the_status_follows_the_verdict() -> Result<(), Box<dyn std::error::Error>> {
    let pool = common::test_pool("CROWDRELAY_TEST_DATABASE_URL").await?;
    for (current, verdict, expected) in [
        ("promoted", Some("refused"), "proposed"),
        ("proposed", Some("admitted"), "promoted"),
        ("discarded", Some("admitted"), "discarded"),
        ("discarded", Some("refused"), "discarded"),
        ("promoted", None, "promoted"),
        ("proposed", None, "proposed"),
    ] {
        let status: String = sqlx::query_scalar("SELECT community_target_status($1, $2)")
            .bind(current)
            .bind(verdict)
            .fetch_one(&pool)
            .await?;
        assert_eq!(status, expected, "{current} + {verdict:?}");
    }
    Ok(())
}
