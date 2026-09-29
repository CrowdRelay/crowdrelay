//! Title normalization for the Reddit submit wire payload.
//!
//! Metal subreddits enforce a `TITLE_REGEX` that expects "Band - Title" with
//! an ASCII hyphen-minus. Drafted titles arrive with typographic dashes — the
//! drafter writes "Virya – Technophobia" with an en dash — and Reddit rejects
//! the post with `SUBMIT_VALIDATION_TITLE_REGEX_REQUIREMENT`. Only the payload
//! sees the normalized form; the stored `community_posts.title` keeps what the
//! drafter wrote. Every other Unicode character survives: a title like
//! "Live in Namysłów" must keep its diacritics, so this maps dashes and
//! nothing else.

use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

/// Returns the title with every typographic dash folded to ASCII `-` and the
/// result trimmed: U+2012 figure dash, U+2013 en dash, U+2014 em dash and
/// U+2212 minus sign.
pub(super) fn reddit_title(title: &str) -> String {
    crowdrelay_domain::reddit_title::ascii_dashes(title)
}

/// Rewrites seeded `community_posts` titles to the format each community's
/// stored rules declare — runs inside the claim transaction right after the
/// draft becomes a post row, so the stored title (the one the manual queue
/// shows a person) is already compliant instead of failing `TITLE_REGEX`
/// days later. `title_for` answers `None` for communities whose rules say
/// nothing about titles, so only format-declaring subs are touched. The
/// update also repairs rows still waiting from before the format was known.
///
/// `band` is the workspace's own name; `track` and `year` split out of the
/// draft with `split_draft_title`, the draft's bare "(2026)" winning over
/// the source's publish year when both exist. Genre is `None` — no table
/// holds the tenant's genre yet, so a "[genre]" slot goes out unfilled
/// rather than tagged wrong.
pub(super) async fn enforce_title_formats(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: Uuid,
) -> Result<usize, sqlx::Error> {
    let band: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = $1")
        .bind(workspace_id)
        .fetch_one(&mut **tx)
        .await?;
    let rows: Vec<(Uuid, String, String, Option<OffsetDateTime>)> = sqlx::query_as(
        r#"
        SELECT cp.id, cp.title, rules.rules_summary, cs.occurred_at
        FROM community_posts cp
        JOIN discovery_place_rules rules
          ON rules.place_id = cp.place_id
        LEFT JOIN content_sources cs
          ON cs.workspace_id = cp.workspace_id
         AND cs.id = cp.relay_source_id
        WHERE cp.workspace_id = $1
          AND cp.platform = 'reddit'
          AND cp.status IN ('pending', 'rate_limited', 'awaiting_manual_post')
          AND rules.rules_summary IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut rewritten = 0usize;
    for (post_id, draft, summary, occurred_at) in rows {
        let requirements = crowdrelay_domain::reddit_title::title_requirements(&summary);
        if !requirements.format {
            continue;
        }
        let parts = crowdrelay_domain::reddit_title::split_draft_title(&draft, &band);
        let year = parts.year.or_else(|| occurred_at.map(|stamp| stamp.year()));
        // When the rules demand a bare "(Year)", `title_for` strips the
        // descriptor itself; otherwise the descriptor stays attached to the
        // track so "Informative Titles" subs keep "(Live From FLSS 2026)".
        let track = match (&parts.descriptor, requirements.year) {
            (Some(descriptor), false) => format!("{} {}", parts.track, descriptor),
            _ => parts.track.clone(),
        };
        let Some(title) =
            crowdrelay_domain::reddit_title::title_for(Some(&summary), &band, &track, year, None)
        else {
            continue;
        };
        if title == draft {
            continue;
        }
        sqlx::query("UPDATE community_posts SET title = $3, updated_at = now() WHERE id = $1 AND workspace_id = $2")
            .bind(post_id)
            .bind(workspace_id)
            .bind(&title)
            .execute(&mut **tx)
            .await?;
        rewritten += 1;
    }
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::reddit_title;

    #[test]
    fn en_dash_becomes_ascii_hyphen() {
        assert_eq!(
            reddit_title("Virya \u{2013} Technophobia (Live From FLSS 2026) [death metal]"),
            "Virya - Technophobia (Live From FLSS 2026) [death metal]"
        );
    }

    #[test]
    fn em_dash_and_minus_sign_become_ascii_hyphen() {
        assert_eq!(reddit_title("Virya \u{2014} Rise"), "Virya - Rise");
        assert_eq!(reddit_title("Virya \u{2212} Rise"), "Virya - Rise");
        assert_eq!(reddit_title("Virya \u{2012} Rise"), "Virya - Rise");
    }

    #[test]
    fn other_unicode_survives() {
        assert_eq!(
            reddit_title("Virya \u{2013} Rise (Live in Namys\u{0142}\u{00f3}w)"),
            "Virya - Rise (Live in Namys\u{0142}\u{00f3}w)"
        );
    }

    #[test]
    fn ascii_title_is_unchanged_and_trimmed() {
        assert_eq!(
            reddit_title("Virya - Technophobia [death metal]"),
            "Virya - Technophobia [death metal]"
        );
        assert_eq!(reddit_title("  Virya - Rise  "), "Virya - Rise");
    }
}
