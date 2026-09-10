//! What a machine is allowed to publish under the tenant's name.
//!
//! Every outbound channel drafts and waits for a person. That person is the
//! throughput limit on fan growth, and removing them is the single largest
//! change available — but the person was also doing something nobody else
//! does: reading the post before it went out under the band's name.
//!
//! This module is what replaces that read. It is the difference between
//! autonomous publishing and unattended publishing.
//!
//! # What it is, and what it is not
//!
//! Deterministic, pure, and mechanical. It cannot judge whether a post is
//! good, interesting, or on-topic; a model wrote it and only a model or a
//! person can assess that. What it can do is refuse the failure modes that
//! are cheap to detect and expensive to publish: a link the system did not
//! issue, an unfilled template, a wall of capitals, a post so short it says
//! nothing, contact details nobody approved.
//!
//! A model asked to review its own output will approve it. That is why the
//! checks here read the text rather than asking anything, and why the verdict
//! is [`PublishVerdict::HoldForHuman`] rather than "discard" — a refused post
//! goes to the operator queue with its reason, which is exactly where it was
//! before autonomy, and no worse.
//!
//! # Fail closed
//!
//! Anything this module cannot establish is a hold, not a publish. The cost
//! of holding is a post that goes out late. The cost of publishing is a post
//! that cannot be recalled, sent to a community that can ban the account the
//! whole growth loop reads through.

use std::collections::BTreeSet;

/// Where a post is going. Limits differ by channel because the audiences and
/// the platforms do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishChannel {
    /// The tenant's own Telegram channel, via the Bot API.
    Telegram,
    /// The tenant's own Discord server, via a webhook or bot.
    Discord,
    /// The tenant's own Instagram Professional account.
    ///
    /// Separate from `Social` because the limit is: Instagram allows a 2,200
    /// character caption where X allows 280, and holding every Instagram post
    /// that ran past a tweet's length would be the guard enforcing the wrong
    /// platform's rule.
    Instagram,
    /// A third-party social platform.
    Social,
}

impl PublishChannel {
    /// The shortest post worth publishing on this channel.
    ///
    /// A two-word post is not a short post; it is a model that produced
    /// nothing and a pipeline that shipped it. The floors are deliberately
    /// low — the guard refuses emptiness, not brevity.
    const fn minimum_characters(self) -> usize {
        match self {
            Self::Telegram | Self::Discord | Self::Instagram => 40,
            // A social post is shorter by convention and by platform limit.
            Self::Social => 20,
        }
    }

    /// The longest post the platform accepts without truncating it.
    ///
    /// Publishing something that will be cut mid-sentence is worse than
    /// holding it: the reader sees a broken post and the smart link, which is
    /// usually last, never arrives.
    const fn maximum_characters(self) -> usize {
        match self {
            // Telegram's sendMessage limit is 4096.
            Self::Telegram => 4_000,
            // Discord's message limit is 2000.
            Self::Discord => 1_900,
            // Instagram's caption limit is 2200.
            Self::Instagram => 2_100,
            // The tightest common social limit.
            Self::Social => 280,
        }
    }
}

/// What the guard decided about one drafted post.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishVerdict {
    /// Nothing mechanical is wrong with it. Publish.
    Publish,
    /// Hold it for a person, and tell them why.
    ///
    /// The reason is written for an operator reading a queue, not for a log
    /// grep: it says what is wrong with this post, not which check fired.
    HoldForHuman(HoldReason),
}

impl PublishVerdict {
    #[must_use]
    pub const fn is_publish(&self) -> bool {
        matches!(self, Self::Publish)
    }

    /// The stored reason, or `None` when the verdict is to publish.
    #[must_use]
    pub const fn hold_reason(&self) -> Option<HoldReason> {
        match self {
            Self::Publish => None,
            Self::HoldForHuman(reason) => Some(*reason),
        }
    }
}

/// Why a post was held. A closed set: each one is a distinct thing an operator
/// can look at and act on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldReason {
    /// Nothing to publish.
    Empty,
    /// Shorter than the channel's floor — the model produced a fragment.
    TooShort,
    /// Longer than the platform accepts; publishing truncates it.
    TooLong,
    /// The post carries a link the system did not issue.
    ///
    /// The single most dangerous thing in a machine-written post. The smart
    /// link is how a fan is attributed and how the loop measures itself; any
    /// other URL is a destination nobody reviewed, and a model that
    /// hallucinates one sends the audience there under the band's name.
    UnapprovedLink,
    /// More than one link. Even when every link is approved, a post that is
    /// mostly links reads as spam to a human and to a platform's filter.
    TooManyLinks,
    /// An unfilled template or scaffold survived into the draft.
    UnfilledTemplate,
    /// Contact details in the body. A machine must not publish an address or
    /// a number under the tenant's name.
    ContactDetails,
    /// Shouting. A high ratio of capitals is the oldest spam signal there is.
    ExcessiveCapitals,
    /// Punctuation flooding — repeated exclamation or question marks.
    ExcessivePunctuation,
    /// The same post, already published recently.
    ///
    /// A model with a fixed prompt and a stable world produces the same text
    /// twice. Publishing it twice is what a bot does.
    DuplicateOfRecentPost,
}

impl HoldReason {
    /// The sentence an operator reads in the draft queue.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "held: the draft is empty",
            Self::TooShort => "held: too short to say anything",
            Self::TooLong => "held: longer than the platform accepts, and would be truncated",
            Self::UnapprovedLink => "held: contains a link the system did not issue",
            Self::TooManyLinks => "held: more than one link",
            Self::UnfilledTemplate => {
                "held: an unfilled template placeholder survived into the draft"
            }
            Self::ContactDetails => "held: contains contact details",
            Self::ExcessiveCapitals => "held: mostly capital letters",
            Self::ExcessivePunctuation => "held: repeated exclamation or question marks",
            Self::DuplicateOfRecentPost => "held: identical to a post already published",
        }
    }
}

/// What the guard needs to know beyond the text itself.
#[derive(Clone, Debug)]
pub struct PublishContext<'a> {
    pub channel: PublishChannel,
    /// The origins a link in this post may point at: the tenant's own site and
    /// the smart-link origin that attributes a fan back to the post.
    ///
    /// An allowlist of origins rather than one exact URL, because a band
    /// legitimately links its own release page, and a post that may carry only
    /// a tracked link would be held every time it did. Matched as a prefix
    /// against the whole URL, so `evil.example/?next=<origin>` does not pass —
    /// the origin has to be where the link starts, not something it mentions.
    ///
    /// Empty means no link may be published at all.
    pub approved_origins: &'a [&'a str],
    /// Content hashes of what this channel published recently. A draft whose
    /// hash is in here is a repeat.
    pub recent_content_hashes: &'a BTreeSet<String>,
}

/// The share of letters that may be capitals before a post reads as shouting.
///
/// Measured over letters only: digits, punctuation and emoji are not shouting,
/// and counting them made a post full of dates look like a post full of
/// capitals. Short posts are exempt — "OUT NOW" is three quarters capitals and
/// entirely normal.
const MAX_CAPITAL_RATIO: f64 = 0.5;
/// Below this length the capitals ratio is not evidence of anything.
const CAPITALS_RATIO_APPLIES_ABOVE: usize = 40;
/// Consecutive `!` or `?` that reads as flooding rather than emphasis.
const MAX_REPEATED_PUNCTUATION: usize = 3;

/// Scaffolding that must never survive into a published post.
///
/// Lowercased before matching. Each of these has one meaning: a template the
/// model did not fill, or filler it emitted instead of content.
const TEMPLATE_MARKERS: &[&str] = &[
    "{{",
    "}}",
    "[insert",
    "[your",
    "[band",
    "[link]",
    "todo:",
    "tbd",
    "lorem ipsum",
    "as an ai",
    "i cannot",
    "placeholder",
];

/// Reviews one drafted post.
///
/// Returns [`PublishVerdict::Publish`] only when every mechanical check
/// passes. Everything else is a hold with the reason, which the caller records
/// beside the draft so the operator queue explains itself.
#[must_use]
pub fn review_outbound_post(body: &str, context: &PublishContext<'_>) -> PublishVerdict {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return PublishVerdict::HoldForHuman(HoldReason::Empty);
    }
    let length = trimmed.chars().count();
    if length < context.channel.minimum_characters() {
        return PublishVerdict::HoldForHuman(HoldReason::TooShort);
    }
    if length > context.channel.maximum_characters() {
        return PublishVerdict::HoldForHuman(HoldReason::TooLong);
    }

    let lowered = trimmed.to_lowercase();
    if TEMPLATE_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        return PublishVerdict::HoldForHuman(HoldReason::UnfilledTemplate);
    }

    let links = extract_links(trimmed);
    if links.len() > 1 {
        return PublishVerdict::HoldForHuman(HoldReason::TooManyLinks);
    }
    if let Some(link) = links.first() {
        let approved = context
            .approved_origins
            .iter()
            .any(|origin| !origin.is_empty() && link.starts_with(origin));
        if !approved {
            return PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink);
        }
    }

    if contains_contact_details(trimmed) {
        return PublishVerdict::HoldForHuman(HoldReason::ContactDetails);
    }
    if length > CAPITALS_RATIO_APPLIES_ABOVE && capital_ratio(trimmed) > MAX_CAPITAL_RATIO {
        return PublishVerdict::HoldForHuman(HoldReason::ExcessiveCapitals);
    }
    if has_punctuation_flood(trimmed) {
        return PublishVerdict::HoldForHuman(HoldReason::ExcessivePunctuation);
    }
    if context
        .recent_content_hashes
        .contains(&content_hash(trimmed))
    {
        return PublishVerdict::HoldForHuman(HoldReason::DuplicateOfRecentPost);
    }

    PublishVerdict::Publish
}

/// A stable identity for a post's text, for duplicate detection.
///
/// Whitespace-normalised and lowercased: a model that re-emits the same post
/// with a different line break has not written a different post.
#[must_use]
pub fn content_hash(body: &str) -> String {
    use sha2::{Digest, Sha256};

    let normalised = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = Sha256::digest(normalised.to_lowercase().as_bytes());
    let mut hash = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}

/// Every URL in the body, wherever it appears inside a token.
///
/// Scans for the scheme anywhere rather than only at a whitespace boundary.
/// The first version split on whitespace and kept tokens that *started* with a
/// scheme, which meant three ordinary ways of writing a link went unseen:
///
/// - `[listen](https://elsewhere.example)` — markdown, which Telegram and
///   Discord both render, and which is what a model writes when asked for a
///   post with a link
/// - `<a href="https://elsewhere.example">` — HTML, same
/// - `Listen:https://elsewhere.example` — a missing space
///
/// Each of those published an unapproved destination under the tenant's name
/// while the guard reported the post carried no link at all. The whole point
/// of the check is that a hallucinated URL never goes out, so the extractor
/// has to be at least as good at finding links as the renderer is.
fn extract_links(body: &str) -> Vec<&str> {
    /// Where a URL stops. Whitespace ends it, and so do the delimiters a link
    /// is wrapped in — otherwise `(https://example.com/x)` keeps the closing
    /// bracket and no longer matches the origin it should have matched.
    fn is_terminator(c: char) -> bool {
        c.is_whitespace() || matches!(c, ')' | ']' | '>' | '"' | '\'' | '`' | ',' | ';')
    }

    // `get` rather than indexing throughout: a post carries emoji and Polish
    // text, and a byte index that is not a character boundary panics. The
    // indices here come from `find` on ASCII needles and are boundaries in
    // practice, but a guard that panics on a post is worse than one that
    // misses a link, and `get` makes that impossible rather than unlikely.
    let mut links = Vec::new();
    let mut cursor = 0usize;
    while let Some(rest) = body.get(cursor..) {
        let Some(offset) = ["https://", "http://", "www."]
            .iter()
            .filter_map(|scheme| rest.find(scheme))
            .min()
        else {
            break;
        };
        let start = cursor + offset;
        let Some(from_start) = body.get(start..) else {
            break;
        };
        let end = from_start
            .find(is_terminator)
            .map_or(body.len(), |length| start + length);
        // Trailing sentence punctuation is not part of the URL.
        let link = body
            .get(start..end)
            .unwrap_or_default()
            .trim_end_matches(['.', '!', '?', ':']);
        if !link.is_empty() {
            links.push(link);
        }
        cursor = end.max(start + 1);
    }
    links
}

/// Whether the body carries an email address or a phone-shaped run of digits.
///
/// Deliberately blunt. A false positive costs a hold; a false negative
/// publishes someone's contact details under the tenant's name.
fn contains_contact_details(body: &str) -> bool {
    let has_email = body.split_whitespace().any(|token| {
        let token = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        // Not a full address parser: an `@` with text and a dotted domain
        // after it is enough to hold the post and let a person look.
        matches!(token.split_once('@'), Some((local, domain)) if
            !local.is_empty() && domain.contains('.') && !domain.ends_with('.'))
    });
    if has_email {
        return true;
    }
    // A run of 9 or more digits, ignoring the separators a phone number uses.
    // Shorter runs are dates, prices and catalogue numbers.
    let mut run = 0usize;
    for character in body.chars() {
        if character.is_ascii_digit() {
            run += 1;
            if run >= 9 {
                return true;
            }
        } else if !matches!(character, ' ' | '-' | '(' | ')' | '+' | '.') {
            run = 0;
        }
    }
    false
}

/// The share of alphabetic characters that are uppercase.
fn capital_ratio(body: &str) -> f64 {
    let letters = body.chars().filter(|c| c.is_alphabetic()).count();
    if letters == 0 {
        return 0.0;
    }
    let capitals = body.chars().filter(|c| c.is_uppercase()).count();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a post is bounded at a few thousand characters; f64 is exact well past that"
    )]
    let ratio = capitals as f64 / letters as f64;
    ratio
}

/// Whether `!` or `?` repeats beyond emphasis into flooding.
fn has_punctuation_flood(body: &str) -> bool {
    let mut run = 0usize;
    let mut last = '\0';
    for character in body.chars() {
        if matches!(character, '!' | '?') {
            if character == last {
                run += 1;
            } else {
                run = 1;
            }
            if run > MAX_REPEATED_PUNCTUATION {
                return true;
            }
        } else {
            run = 0;
        }
        last = character;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINK: &str = "https://virya.music/l/spring-tour";

    const ORIGINS: &[&str] = &["https://virya.music"];

    fn context(channel: PublishChannel) -> PublishContext<'static> {
        static EMPTY: std::sync::LazyLock<BTreeSet<String>> =
            std::sync::LazyLock::new(BTreeSet::new);
        PublishContext {
            channel,
            approved_origins: ORIGINS,
            recent_content_hashes: &EMPTY,
        }
    }

    fn good_post() -> String {
        format!(
            "New single out this Friday. We recorded it live in one take at \
             the old cinema in Wroclaw. Listen here: {LINK}"
        )
    }

    #[test]
    fn a_normal_post_publishes() {
        assert_eq!(
            review_outbound_post(&good_post(), &context(PublishChannel::Telegram)),
            PublishVerdict::Publish
        );
    }

    #[test]
    fn a_link_the_system_did_not_issue_is_held() {
        let body = good_post().replace(LINK, "https://totally-not-us.example/track");
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink),
            "a hallucinated destination must never publish under the tenant's name"
        );
    }

    #[test]
    fn a_link_that_merely_contains_the_approved_one_is_held() {
        let body = format!(
            "New single out this Friday, recorded live in one take at the old \
             cinema. Listen: https://evil.example/?next={LINK}"
        );
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink)
        );
    }

    /// The ways a link is written that are not "scheme at a word boundary".
    ///
    /// Telegram and Discord render markdown, so `[text](url)` is not an exotic
    /// input — it is what a model produces when asked for a post with a link.
    /// An extractor that misses these reports "no link in this post" about a
    /// post that publishes someone else's URL under the tenant's name.
    #[test]
    fn a_link_hidden_inside_a_token_is_still_a_link() {
        let disguises = [
            "New single out Friday, recorded live in one take. \
             Listen: [right here](https://elsewhere.example/track)",
            "New single out Friday, recorded live in one take. \
             Listen:https://elsewhere.example/track",
            "New single out Friday, recorded live in one take. \
             <a href=\"https://elsewhere.example/track\">listen</a>",
            "New single out Friday, recorded live in one take. \
             Listen at www.elsewhere.example/track",
        ];
        for body in disguises {
            assert_eq!(
                review_outbound_post(body, &context(PublishChannel::Telegram)),
                PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink),
                "this link went unseen: {body}"
            );
        }
    }

    /// A post is Polish, and Polish has multi-byte characters.
    ///
    /// The extractor walks byte offsets from `find`. Slicing a byte index that
    /// falls inside a character panics, and a guard that panics on a post is
    /// worse than one that misses a link — it takes the executor down instead
    /// of holding one draft.
    #[test]
    fn a_post_with_multibyte_text_does_not_panic() {
        let body = format!(
            "Nowy singiel w piątek — nagraliśmy go na żywo w starym kinie we \
             Wrocławiu. Posłuchaj tutaj: {LINK} 🎸"
        );
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::Publish
        );
        let with_foreign_link = body.replace(LINK, "https://gdzieś-indziej.example/utwór");
        assert_eq!(
            review_outbound_post(&with_foreign_link, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink)
        );
    }

    #[test]
    fn an_approved_link_in_markdown_still_publishes() {
        let body = format!(
            "New single out this Friday, recorded live in one take at the old \
             cinema. Listen: [right here]({LINK})"
        );
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::Publish,
            "the closing bracket must not be read as part of the URL"
        );
    }

    #[test]
    fn an_unfilled_template_is_held() {
        let body = "Hey {{community}}, our new single is out this Friday and we would \
                    love for you to hear it. Listen here."
            .to_owned();
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::UnfilledTemplate)
        );
    }

    #[test]
    fn a_refusal_the_model_wrote_about_itself_is_held() {
        let body = "As an AI language model I cannot write promotional content, \
                    however here is a post about the new single out on Friday."
            .to_owned();
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::UnfilledTemplate)
        );
    }

    #[test]
    fn shouting_is_held_but_a_short_announcement_is_not() {
        let shouted = "OUR NEW SINGLE IS OUT THIS FRIDAY AND YOU HAVE TO HEAR IT \
                       RIGHT NOW BECAUSE IT IS THE BEST THING WE HAVE EVER DONE"
            .to_owned();
        assert_eq!(
            review_outbound_post(&shouted, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::ExcessiveCapitals)
        );
        // Under the length floor the ratio says nothing, so this is held for
        // being short rather than for being capitals — the point is that the
        // ratio did not decide it.
        let short = "OUT NOW".to_owned();
        assert_eq!(
            review_outbound_post(&short, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::TooShort)
        );
    }

    #[test]
    fn punctuation_flooding_is_held() {
        let body = good_post().replace("Friday.", "Friday!!!!");
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::ExcessivePunctuation)
        );
    }

    #[test]
    fn contact_details_are_held() {
        let body = format!("New single out Friday, write to booking@virya.music for shows. {LINK}");
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::ContactDetails)
        );
        let phone = format!("New single out Friday, call us on +48 600 123 456 to book. {LINK}");
        assert_eq!(
            review_outbound_post(&phone, &context(PublishChannel::Telegram)),
            PublishVerdict::HoldForHuman(HoldReason::ContactDetails)
        );
    }

    #[test]
    fn a_date_is_not_a_phone_number() {
        let body = format!(
            "New single out on 2026-09-13, recorded live in one take at the old \
             cinema in Wroclaw. Listen here: {LINK}"
        );
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::Publish
        );
    }

    #[test]
    fn a_repeat_of_a_recent_post_is_held() {
        let body = good_post();
        let mut recent = BTreeSet::new();
        recent.insert(content_hash(&body));
        let context = PublishContext {
            channel: PublishChannel::Telegram,
            approved_origins: ORIGINS,
            recent_content_hashes: &recent,
        };
        assert_eq!(
            review_outbound_post(&body, &context),
            PublishVerdict::HoldForHuman(HoldReason::DuplicateOfRecentPost)
        );
    }

    #[test]
    fn reformatting_does_not_make_a_repeat_a_new_post() {
        let body = good_post();
        let mut recent = BTreeSet::new();
        recent.insert(content_hash(&body));
        let reformatted = body.replace(". ", ".\n\n").to_uppercase();
        let context = PublishContext {
            channel: PublishChannel::Telegram,
            approved_origins: ORIGINS,
            recent_content_hashes: &recent,
        };
        assert_eq!(
            content_hash(&reformatted),
            content_hash(&body),
            "line breaks and case are not a different post"
        );
        // It is held either way; the assertion above is the one that matters.
        assert!(!review_outbound_post(&reformatted, &context).is_publish());
    }

    #[test]
    fn a_post_over_the_platform_limit_is_held() {
        let body = "a".repeat(2_500);
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Discord)),
            PublishVerdict::HoldForHuman(HoldReason::TooLong)
        );
        assert_eq!(
            review_outbound_post(&body, &context(PublishChannel::Telegram)),
            PublishVerdict::Publish,
            "Telegram accepts what Discord truncates; the limit is per channel"
        );
    }

    #[test]
    fn with_no_approved_origin_no_link_may_be_published() {
        let context = PublishContext {
            channel: PublishChannel::Telegram,
            approved_origins: &[],
            recent_content_hashes: &BTreeSet::new(),
        };
        let plain = "New single out this Friday. We recorded it live in one take \
                     at the old cinema in Wroclaw."
            .to_owned();
        assert_eq!(
            review_outbound_post(&plain, &context),
            PublishVerdict::Publish
        );
        assert_eq!(
            review_outbound_post(&good_post(), &context),
            PublishVerdict::HoldForHuman(HoldReason::UnapprovedLink),
            "with no approved origin, every link in the body is unapproved"
        );
    }
}
