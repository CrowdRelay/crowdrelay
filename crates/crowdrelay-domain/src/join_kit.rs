//! The standing join kit (FAN_100 §B4) — one tracked link per permanent
//! social placement, kept separate from the weekly rotating post.
//!
//! The weekly join-ask rotates a post through its own `cta_url`; these are
//! the slots a post never reaches — the bio, the pinned comment, the channel
//! description — pasted once by the band and left. Each placement gets its
//! own slug so the acquisition readout can tell a bio click from a story
//! click instead of folding every social visit into one "instagram" bucket.

/// One standing placement in the join kit — the link the band pastes into a
/// bio, a pinned comment or a channel description once and leaves there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JoinKitPlacement {
    /// Channel source the link attributes under — the same spellings the
    /// join-ask lane writes (`instagram`, `facebook`, `youtube`).
    pub platform: &'static str,
    /// The slot inside the platform, recorded as `channel_community`.
    pub placement: &'static str,
    /// The printable slug under `/l/`.
    pub slug: &'static str,
    /// One line for the operator card: where on the platform this link goes.
    pub hint: &'static str,
}

/// The full placement set the kit endpoint ensures and reports on.
///
/// The list is deliberately short and boring — the bio a follower sees
/// before a post, the comment pinned under a post or video, the description
/// a channel carries forever. Instagram has no linkable feed post, so its
/// slots are the bio, the story sticker and a comment under the pinned
/// reel; Facebook's are the page intro, a standing post and a pinned
/// comment; YouTube's are the channel description and a pinned comment.
pub const JOIN_KIT_PLACEMENTS: &[JoinKitPlacement] = &[
    JoinKitPlacement {
        platform: "instagram",
        placement: "bio",
        slug: "join-ig-bio",
        hint: "the link in the Instagram profile",
    },
    JoinKitPlacement {
        platform: "instagram",
        placement: "story",
        slug: "join-ig-story",
        hint: "the link sticker on a story",
    },
    JoinKitPlacement {
        platform: "instagram",
        placement: "comment",
        slug: "join-ig-comment",
        hint: "a comment under the pinned reel",
    },
    JoinKitPlacement {
        platform: "facebook",
        placement: "page",
        slug: "join-fb-page",
        hint: "the page's intro/about link",
    },
    JoinKitPlacement {
        platform: "facebook",
        placement: "post",
        slug: "join-fb-post",
        hint: "the pinned post",
    },
    JoinKitPlacement {
        platform: "facebook",
        placement: "comment",
        slug: "join-fb-comment",
        hint: "a comment pinned under the pinned post",
    },
    JoinKitPlacement {
        platform: "youtube",
        placement: "desc",
        slug: "join-yt-desc",
        hint: "the channel description and new video descriptions",
    },
    JoinKitPlacement {
        platform: "youtube",
        placement: "comment",
        slug: "join-yt-comment",
        hint: "a pinned comment under the latest video",
    },
];

/// The destination every kit link points at — the member site's `/signal`
/// page tagged so the landing knows which placement sent it.
///
/// Attribution in the acquisition ledger reads the link row's channel
/// fields, not this query string; the tags exist so the page's own
/// first-party view of the visit tells the same story the ledger does —
/// the same convention the weekly posts' `cta_url` follows.
#[must_use]
pub fn join_kit_destination(site_root: &str, placement: &JoinKitPlacement) -> String {
    format!(
        "{}/signal?utm_source={}&utm_medium={}&utm_campaign=join_kit",
        site_root.trim_end_matches('/'),
        placement.platform,
        placement.placement,
    )
}
