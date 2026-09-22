/// Whether `base` already inlines the `/l/` link: an occurrence of `link`
/// whose next character is not a slug-continuation character. A plain
/// `contains` would confuse `/l/abc` with `/l/abc-def` — a draft naming the
/// longer link must not suppress appending the shorter one it was bound to.
/// A trailing `?`, `/`, `)`, quote or whitespace means the draft really did
/// name this link.
pub(crate) fn link_is_inlined(base: &str, link: &str) -> bool {
    base.match_indices(link).any(|(idx, _)| {
        base.get(idx + link.len()..)
            .and_then(|rest| rest.chars().next())
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
    })
}

#[cfg(test)]
mod tests {
    use super::link_is_inlined;

    #[test]
    fn a_longer_slug_does_not_mask_the_shorter_one() {
        assert!(!link_is_inlined(
            "see https://virya.music/l/abc-def",
            "/l/abc"
        ));
    }

    #[test]
    fn the_exact_slug_is_recognized_as_inlined() {
        assert!(link_is_inlined("see /l/abc now", "/l/abc"));
        assert!(link_is_inlined("see /l/abc?utm=ig", "/l/abc"));
        assert!(link_is_inlined("see https://virya.music/l/abc", "/l/abc"));
    }
}
