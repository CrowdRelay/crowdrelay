//! Spotify top-city resolution against the `cities` catalog.
//!
//! Spotify's pathfinder `topCities` block reports where an artist's monthly
//! listeners live, but names cities the way the English-speaking web player
//! displays them — undiacriticised spellings and English exonyms. The
//! catalog carries local names, so joining needs both a diacritic fold and
//! the small set of cases where the English name is a different word
//! entirely ("Warsaw" vs "Warszawa").
//!
//! The honest failure mode is a skip, not a guess: a city that resolves to
//! nothing, or to more than one catalog row, is logged and dropped. A wrong
//! join would route gig planning toward the wrong city — worse than a gap.

use super::GrowthMetricSyncError;
use sqlx::PgPool;
use uuid::Uuid;

/// Spotify names cities with English exonyms, not the local name the
/// `cities` catalog carries — "Warsaw" is not a diacritic fold of
/// "Warszawa", so folding alone cannot join it. The fold covers
/// Wroclaw→Wrocław, Poznan→Poznań and most of the catalog; this map covers
/// the remainder where the English name is a different word entirely.
/// Kept deliberately short: an unresolved city is skipped with a warning
/// rather than guessed, so a missing entry costs one logged skip, not a
/// wrong join.
fn spotify_city_endonym<'a>(country_code: &str, name: &'a str) -> &'a str {
    // Spotify's casing is not contractual — match on the lowered name so a
    // "warsaw" does not slip past the map and fold to nothing.
    match (country_code, name.to_lowercase().as_str()) {
        ("PL", "warsaw") => "Warszawa",
        ("PL", "cracow") => "Kraków",
        ("DE", "munich") => "München",
        ("DE", "cologne") => "Köln",
        ("DE", "nuremberg") => "Nürnberg",
        ("DE", "hanover") => "Hannover",
        ("AT", "vienna") => "Wien",
        ("CZ", "prague") => "Praha",
        _ => name,
    }
}

/// Lowercases and folds the same diacritics the `resolve_spotify_city`
/// query's `translate` removes from the catalog name. The two character
/// lists are one list written twice — extend both or neither.
fn fold_spotify_city_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| match c {
            'ą' => 'a',
            'ć' => 'c',
            'ę' => 'e',
            'ł' => 'l',
            'ń' => 'n',
            'ó' => 'o',
            'ś' => 's',
            'ź' | 'ż' => 'z',
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            'ß' => 's',
            'é' | 'è' | 'ě' => 'e',
            'á' => 'a',
            'í' => 'i',
            'ř' => 'r',
            'š' => 's',
            'č' => 'c',
            'ž' => 'z',
            'ď' => 'd',
            'ť' => 't',
            'ň' => 'n',
            'ů' | 'ú' => 'u',
            'ý' => 'y',
            other => other,
        })
        .collect()
}

/// Resolve a Spotify top-city name to a `cities` row, scoped to the country
/// Spotify reported. Returns `None` when the name resolves to nothing or to
/// more than one row — an ambiguous join is a wrong join, and a missing
/// city series is the honest answer. Public so the postgres suite can
/// assert the fold against a real catalog row.
pub async fn resolve_spotify_city(
    pool: &PgPool,
    country_code: &str,
    spotify_name: &str,
) -> Result<Option<(Uuid, String)>, GrowthMetricSyncError> {
    let folded = fold_spotify_city_name(spotify_city_endonym(country_code, spotify_name));
    // `translate` is positional: each character in the second argument maps
    // to the same position in the third. The list mirrors
    // `fold_spotify_city_name` exactly.
    let matches: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT id, name
        FROM cities
        WHERE country_code = $1
          AND translate(
              lower(name),
              'ąćęłńóśźżäöüßéèěáířščžďťňůúý',
              'acelnoszzaouseeeairsczdtnuuy'
          ) = $2
          -- A merged or rejected row is not a joinable city — the series
          -- would annotate a row the catalog has already disowned.
          AND moderation_status IN ('approved', 'pending')
        ORDER BY name
        LIMIT 2
        "#,
    )
    .bind(country_code)
    .bind(&folded)
    .fetch_all(pool)
    .await?;
    match matches.as_slice() {
        [single] => Ok(Some(single.clone())),
        [] => Ok(None),
        _ => {
            tracing::warn!(
                country_code,
                spotify_name,
                candidates = ?matches.iter().map(|(_, name)| name).collect::<Vec<_>>(),
                "spotify top city resolved ambiguously — skipped"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_city_names_fold_diacritics() {
        assert_eq!(fold_spotify_city_name("Wrocław"), "wroclaw");
        assert_eq!(fold_spotify_city_name("Poznań"), "poznan");
        assert_eq!(fold_spotify_city_name("Łódź"), "lodz");
        assert_eq!(fold_spotify_city_name("Gdańsk"), "gdansk");
        assert_eq!(fold_spotify_city_name("München"), "munchen");
        assert_eq!(fold_spotify_city_name("Berlin"), "berlin");
    }

    #[test]
    fn spotify_city_exonyms_cover_what_a_fold_cannot() {
        // "Warsaw" is a different word from "Warszawa", not a diacritic
        // variant — folding it produces "warsaw", which matches nothing.
        assert_eq!(spotify_city_endonym("PL", "Warsaw"), "Warszawa");
        assert_eq!(
            fold_spotify_city_name(spotify_city_endonym("PL", "Warsaw")),
            "warszawa"
        );
        // Spotify's casing is not contractual — a lowercase "warsaw" still
        // maps rather than slipping past to a silent miss.
        assert_eq!(spotify_city_endonym("PL", "warsaw"), "Warszawa");
        // Names already in catalog form pass through untouched.
        assert_eq!(spotify_city_endonym("PL", "Wroclaw"), "Wroclaw");
        // The map is country-scoped: a "Warsaw" outside Poland (there is
        // one in Indiana) must not resolve to Warszawa.
        assert_eq!(spotify_city_endonym("US", "Warsaw"), "Warsaw");
    }
}
