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

/// Returns the title with every typographic dash folded to ASCII `-` and the
/// result trimmed: U+2012 figure dash, U+2013 en dash, U+2014 em dash and
/// U+2212 minus sign.
pub(super) fn reddit_title(title: &str) -> String {
    title
        .chars()
        .map(|c| match c {
            '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_owned()
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
