-- Turn docs/GROWTH_INTELLIGENCE_RESEARCH.md into rows the brain can act on.
--
-- The research listed ~65 communities and ~45 outreach targets in markdown
-- tables. The brain reads Postgres, so none of it was reachable: the discovery
-- and outreach contexts loaded an empty world and correctly decided there was
-- nothing to do. This file is that research, transcribed.
--
-- Idempotent. Re-running changes nothing, so it is safe to apply on every
-- deploy and safe to run twice by hand:
--
--   psql "$CROWDRELAY_DATABASE_URL" -v slug=virya -f ops/growth/virya-communities.seed.sql
--
-- Two deliberate omissions:
--
--   Fanbase connections are not seeded. They carry credential_ref and OAuth
--   tokens, which do not belong in a checked-in file. Connect those through
--   the per-platform endpoints (POST /v1/admin/connections/{platform}); the
--   NOTIFY trigger then starts the metric sync on its own.
--
--   Everything here lands as a candidate, not an approved action.
--   discovery_places.status = 'active' means "known and worth scanning";
--   agent_outreach_targets.status = 'proposed' means "the brain may prepare a
--   pitch, an operator decides whether it is sent". Nothing in this file
--   authorizes contacting anyone.
--
-- accepts_outreach is set only where the outlet publishes a submission address
-- or a submission form. do_not_contact stays false everywhere; it exists for
-- operators to set after a "no".

\set ON_ERROR_STOP on

-- Default the target workspace so the file runs bare against a dev database.
\if :{?slug}
\else
\set slug virya
\endif

BEGIN;

-- psql interpolates :'slug' in plain SQL but not inside dollar-quoted bodies,
-- so the slug is carried in a transaction-local setting that both can read.
SET LOCAL seed.slug = :'slug';

-- Fail loudly rather than silently seeding nothing when the slug is wrong.
-- Without this the CROSS JOIN simply matches no workspace and the whole file
-- reports success having inserted nothing at all.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM workspaces WHERE slug = current_setting('seed.slug')) THEN
        RAISE EXCEPTION 'no workspace with slug %', current_setting('seed.slug');
    END IF;
END;
$$;

-- ---------------------------------------------------------------------------
-- Discovery places: where fans already gather.
-- ---------------------------------------------------------------------------

INSERT INTO discovery_places
    (workspace_id, place_kind, platform, name, url, country_code, language, genres, member_count, notes)
SELECT w.id, v.place_kind, v.platform, v.name, v.url, v.country_code, v.language, v.genres, v.member_count, v.notes
FROM workspaces w
CROSS JOIN (VALUES
    -- Reddit. Member counts are from the research pass; NULL where uncounted.
    ('subreddit','reddit','r/Metal','https://reddit.com/r/Metal',NULL,'en',ARRAY['metal'],2600000,'Broad metal (Shreddit). No direct promo — earn trust in discussions first.'),
    ('subreddit','reddit','r/metalcore','https://reddit.com/r/metalcore',NULL,'en',ARRAY['metalcore'],1000000,'Limited self-promo. Weekly new-music thread is the opening.'),
    ('subreddit','reddit','r/deathcore','https://reddit.com/r/deathcore',NULL,'en',ARRAY['deathcore'],190000,'Limited self-promo. Newcomer-friendly recommendation threads.'),
    ('subreddit','reddit','r/progmetal','https://reddit.com/r/progmetal',NULL,'en',ARRAY['progressive metal'],296000,'Recommendation threads only, no direct promo posts.'),
    ('subreddit','reddit','r/progrock','https://reddit.com/r/progrock',NULL,'en',ARRAY['progressive rock'],NULL,NULL),
    ('subreddit','reddit','r/progrockmusic','https://reddit.com/r/progrockmusic',NULL,'en',ARRAY['progressive rock'],NULL,'Sister sub to r/progmetal.'),
    ('subreddit','reddit','r/PowerMetal','https://reddit.com/r/PowerMetal',NULL,'en',ARRAY['power metal'],NULL,NULL),
    ('subreddit','reddit','r/Djent','https://reddit.com/r/Djent',NULL,'en',ARRAY['djent'],NULL,'Direct genre fit.'),
    ('subreddit','reddit','r/ListenToThis','https://reddit.com/r/ListenToThis',NULL,'en',ARRAY['music discovery'],NULL,'Self-promo allowed with proper tags.'),
    ('subreddit','reddit','r/ShareYourMusic','https://reddit.com/r/ShareYourMusic',NULL,'en',ARRAY['music discovery'],NULL,'Dedicated to self-promotion — post directly.'),
    ('subreddit','reddit','r/Polska','https://reddit.com/r/Polska','PL','pl',ARRAY['polish'],NULL,'Polish community. Use for concert announcements, not music spam.'),
    ('subreddit','reddit','r/melodicdeathmetal','https://reddit.com/r/melodicdeathmetal',NULL,'en',ARRAY['melodic death metal'],67000,NULL),
    ('subreddit','reddit','r/metalmemes','https://reddit.com/r/metalmemes',NULL,'en',ARRAY['metal'],NULL,'Culture, not promo. Build presence.'),
    ('subreddit','reddit','r/thrashmetal','https://reddit.com/r/thrashmetal',NULL,'en',ARRAY['thrash metal'],NULL,NULL),
    ('subreddit','reddit','r/doommetal','https://reddit.com/r/doommetal',NULL,'en',ARRAY['doom metal'],NULL,NULL),
    ('subreddit','reddit','r/DeathMetal','https://reddit.com/r/DeathMetal',NULL,'en',ARRAY['death metal'],151000,NULL),
    ('subreddit','reddit','r/BlackMetal','https://reddit.com/r/BlackMetal',NULL,'en',ARRAY['black metal'],NULL,NULL),
    ('subreddit','reddit','r/postmetal','https://reddit.com/r/postmetal',NULL,'en',ARRAY['post metal'],NULL,NULL),
    ('subreddit','reddit','r/stonermetal','https://reddit.com/r/stonermetal',NULL,'en',ARRAY['stoner metal'],NULL,NULL),
    ('subreddit','reddit','r/headbangtothis','https://reddit.com/r/headbangtothis',NULL,'en',ARRAY['metal discovery'],NULL,'Self-promo allowed. Smaller, engaged.'),
    ('subreddit','reddit','r/findnewmetal','https://reddit.com/r/findnewmetal',NULL,'en',ARRAY['metal discovery'],NULL,'Built for discovery — post directly with genre tags.'),
    ('subreddit','reddit','r/poppunkers','https://reddit.com/r/poppunkers',NULL,'en',ARRAY['pop punk'],NULL,'Crossover audience with metalcore. Engage authentically.'),
    ('subreddit','reddit','r/metalvideos','https://reddit.com/r/metalvideos',NULL,'en',ARRAY['metal'],NULL,'Self-promo allowed for music videos.'),
    ('subreddit','reddit','r/posthardcore','https://reddit.com/r/posthardcore',NULL,'en',ARRAY['post-hardcore'],NULL,'Limited self-promo, recommendation threads.'),
    ('subreddit','reddit','r/mathcore','https://reddit.com/r/mathcore',NULL,'en',ARRAY['mathcore'],NULL,NULL),
    ('subreddit','reddit','r/ModernProg','https://reddit.com/r/ModernProg',NULL,'en',ARRAY['progressive metal'],NULL,'Direct genre fit.'),
    ('subreddit','reddit','r/classicmetal','https://reddit.com/r/classicmetal',NULL,'en',ARRAY['classic metal'],NULL,NULL),
    ('subreddit','reddit','r/heavymind','https://reddit.com/r/heavymind',NULL,'en',ARRAY['progressive metal'],NULL,'Thoughtful progressive metal discussion.'),

    -- Discord. Conversational, not promotional; most have a self-promo channel.
    ('discord','discord','Shredders','https://discord.me/shredders',NULL,'en',ARRAY['metal'],NULL,'Largest metalhead Discord. Listening parties, musician''s corner.'),
    ('discord','discord','The Metal Underground','https://discord.me/the-metal-underground',NULL,'en',ARRAY['metal'],NULL,'Nitro-boosted, Last.fm integration, weekly album events, adults-only.'),
    ('discord','discord','Official Metalcore Community','https://disboard.org/server/1490180524948586556',NULL,'en',ARRAY['metalcore','progressive metal','death metal'],NULL,'Direct genre fit.'),
    ('discord','discord','Discore','https://discord.me/dis-core',NULL,'en',ARRAY['metalcore','deathcore','hardcore'],NULL,'Formerly Redditcore. Independent creativity, personal projects.'),
    ('discord','discord','Metal Pilgrim','https://discord.me/metalpilgrim',NULL,'en',ARRAY['metal'],NULL,'Album reviews, new release spotlights.'),
    ('discord','discord','The Pit','https://discord.me/thepitmetal',NULL,'en',ARRAY['rock','metal'],NULL,'International, classic to extreme.'),
    ('discord','discord','Periphery 3DOT','https://disdex.io/server/692904461001293866',NULL,'en',ARRAY['djent','progressive metal'],3700,'Official Periphery server. Closest peer audience.'),
    ('discord','discord','Rebelianci Metalowej Poznajki','https://discord.com/servers/rebelianci-metalowej-poznajki','PL','pl',ARRAY['metal','polish'],NULL,'Largest Polish metal Discord. High value — meetups and concerts.'),

    -- Telegram. Broadcast-only: posting means asking an admin for a feature.
    ('telegram','telegram','MetalWorld','https://t.me/MetalWorld',NULL,'en',ARRAY['metal'],25891,'Largest reach of the channels found.'),
    ('telegram','telegram','Metal Collective','https://t.me/metal_collective',NULL,'en',ARRAY['metal','rock','metalcore'],6556,NULL),
    ('telegram','telegram','Metal Library','https://t.me/metal_library',NULL,'en',ARRAY['metal'],3737,'Discographies.'),
    ('telegram','telegram','Progressive Hate Creation','https://t.me/progressive_hate_creation',NULL,'en',ARRAY['metal','metalcore'],1112,'Close genre fit.'),
    ('telegram','telegram','Metalcore Station','https://t.me/metalcorestation',NULL,'en',ARRAY['metalcore'],1594,NULL),
    ('telegram','telegram','Metalcore Universe','https://t.me/metalcoreuniverse',NULL,'en',ARRAY['metalcore'],778,NULL),
    ('telegram','telegram','Riff Forge','https://t.me/metal_extreme',NULL,'en',ARRAY['metalcore','progressive metalcore'],1600,'Close genre fit.'),
    ('telegram','telegram','MetalTube','https://t.me/MetalCultVideo',NULL,'en',ARRAY['metal'],NULL,'Music videos.'),
    ('telegram','telegram','Prog Metal','https://telegram-channel.net/telegram-channel/prog-metal',NULL,'en',ARRAY['progressive metal'],NULL,NULL),
    ('telegram','telegram','DJENT / METALCORE / PROGRESSIVE','https://telegraminformer.com/channels/djent1984',NULL,'en',ARRAY['djent','metalcore','progressive metal'],NULL,NULL),

    -- Forums. Old-school, loyal, and slow — good for credibility, not volume.
    ('forum','forum','Metal Archives Forum','https://forum.metal-archives.com',NULL,'en',ARRAY['metal'],NULL,'Attached to Encyclopaedia Metallum. Getting Virya listed there is a prerequisite, not an option.'),
    ('forum','forum','Ultimate Metal Forum','https://ultimatemetal.com',NULL,'en',ARRAY['metal'],NULL,'Band-specific subforums — a Virya subforum can be requested.'),
    ('forum','forum','Metal Throne','https://metalthrone.net',NULL,'en',ARRAY['metal'],NULL,NULL),
    ('forum','forum','The Black Vault','https://theblackvault.net',NULL,'en',ARRAY['extreme metal'],5996,'Underground focus. Credibility play.'),
    ('forum','forum','GnarBoard','https://tapatalk.com/groups/braveboard',NULL,'en',ARRAY['heavy metal'],11897,'Allows concert and zine promotion.'),
    ('forum','forum','InMetalWeTrust','https://inmetalwetrust.club',NULL,'en',ARRAY['metal'],NULL,NULL),
    ('forum','forum','Metal Area','https://metalarea.org/forum',NULL,'ru',ARRAY['extreme metal'],NULL,'Russian-language extreme music portal.'),
    ('forum','forum','Slaps — Progressive Metal','https://slaps.com/group/Progressive_Metal',NULL,'en',ARRAY['progressive metal'],1300,NULL),
    ('forum','forum','rockmetal.pl','https://rockmetal.pl','PL','pl',ARRAY['rock','metal','polish'],NULL,'Polish rock/metal news, reviews, concert announcements.'),
    ('forum','forum','Polish Metal Promotion','https://threads.com/@polish_metal_promotion','PL','pl',ARRAY['metal','polish'],NULL,'Promotes Polish underground metal worldwide.'),
    ('forum','forum','Metal Band Promo','https://metalbandpromo.pun.pl','PL','pl',ARRAY['metal','polish'],NULL,'Underground band promotion, battles, contacts.'),
    ('forum','forum','Force of Metal','https://forceofmetal.fora.pl','PL','pl',ARRAY['metal','polish'],NULL,'Polish metal forum with a Polish scene section.'),

    -- Lemmy. Small, but the API is open and the counts are trackable.
    ('lemmy','lemmy','!metalcore@lemmy.ml','https://lemmy.ml/c/metalcore',NULL,'en',ARRAY['metalcore'],NULL,'Subscriber counts readable via /api/v3/community.'),
    ('lemmy','lemmy','!metal@lemmy.world','https://lemmy.world/c/metal',NULL,'en',ARRAY['metal'],NULL,NULL),
    ('lemmy','lemmy','!progmetal@sopuli.xyz','https://sopuli.xyz/c/progmetal',NULL,'en',ARRAY['progressive metal'],NULL,NULL),
    ('lemmy','lemmy','!metal@sopuli.xyz','https://sopuli.xyz/c/metal',NULL,'en',ARRAY['metal'],NULL,NULL),
    ('lemmy','lemmy','!metal@mander.xyz','https://mander.xyz/c/metal',NULL,'en',ARRAY['metal'],NULL,NULL),

    -- Instagram community accounts (the benchmark bands are tracked as metrics,
    -- not as places, so they are deliberately absent here).
    ('instagram','instagram','@metalcorenight','https://instagram.com/metalcorenight',NULL,'en',ARRAY['metalcore','deathcore','post-hardcore'],5700,'Metalcore club nights.'),
    ('instagram','instagram','@MetalBlakedowns','https://instagram.com/metalblakedowns',NULL,'en',ARRAY['metalcore'],15300,'Memes and tour announcements.'),
    ('instagram','instagram','@polish_metal_promotion','https://instagram.com/polish_metal_promotion','PL','pl',ARRAY['metal','polish'],NULL,'Direct fit — promotes Polish underground bands.')
) AS v(place_kind, platform, name, url, country_code, language, genres, member_count, notes)
WHERE w.slug = current_setting('seed.slug')
ON CONFLICT (workspace_id, platform, url) DO NOTHING;

-- ---------------------------------------------------------------------------
-- Outreach targets: people who can put Virya in front of an audience.
-- All land as 'proposed'. The brain drafts; an operator sends.
-- ---------------------------------------------------------------------------

INSERT INTO agent_outreach_targets
    (workspace_id, target_kind, display_name, contact_email, contact_domain, why_fit, verified, accepts_outreach, status)
SELECT w.id, v.target_kind, v.display_name, v.contact_email, v.contact_domain, v.why_fit, v.verified, v.accepts_outreach, v.status
FROM workspaces w
CROSS JOIN (VALUES
    -- Playlist curators. Genre fit is the whole game here.
    ('playlist','Next-Gen Metal',NULL,'groover.co','Progressive metal, core and djent playlist — Virya''s exact niche.',false,true,'proposed'),
    ('playlist','Metal Mayhem',NULL,'groover.co','Metalcore, deathcore and progressive metal playlist.',false,true,'proposed'),
    ('playlist','Metalcore Madness',NULL,'groover.co','Metalcore and post-hardcore, ~10K followers.',false,true,'proposed'),
    ('playlist','At Full Volume',NULL,'groover.co','Metalcore and post-hardcore, 15,431 listeners.',false,true,'proposed'),
    ('playlist','Filipe Almeida',NULL,'groover.co','Runs "All Things Metalcore" and "Metalcore Instrumentals".',false,true,'proposed'),
    ('playlist','METAL POWER GYM',NULL,'groover.co','Metalcore, post-hardcore and alt metal.',false,true,'proposed'),
    ('playlist','Heavy Hearts Club',NULL,'groover.co','Curates "Modern Metal Essentials".',false,true,'proposed'),
    ('playlist','DJENTCORE',NULL,'noiselash.com','Djent and prog metalcore, 11,777 saves. Highest-value single target found.',false,true,'proposed'),
    ('playlist','Tommaso Scalici',NULL,'submitlink.io','Instrumental prog, modern metal, djent and math rock. 2,907 followers, $1 submission.',false,true,'proposed'),
    ('playlist','Yulia Ehwaz',NULL,'submitlink.io','Nine metalcore-focused playlists, 6,248 followers. $2 submission.',false,true,'proposed'),

    -- Press that has already covered Virya. Verified, and a warm door for the
    -- next release rather than a cold pitch.
    ('press','Metal Devastation Radio',NULL,'metaldevastationradio.com','Already covered the "From The Ashes" EP — a warm contact for the next release.',true,true,'proposed'),
    ('press','EuroIndieMusic',NULL,'euroindiemusic.info','Already ran a Virya interview.',true,true,'proposed'),

    -- Major metal press. Cold, high value, pitch 2-3 weeks pre-release.
    ('press','Metal Injection','tips@metalinjection.net','metalinjection.net','Metalcore and progressive coverage. High priority.',false,true,'proposed'),
    ('press','Metal Sucks','news@metalsucks.net','metalsucks.net','Metalcore and modern metal. High priority.',false,true,'proposed'),
    ('press','Loudwire','tips@loudwire.com','loudwire.com','Broad metal reach. High priority.',false,true,'proposed'),
    ('press','Metal Hammer',NULL,'metalhammer.com','Broad metal, UK. Online submission form.',false,true,'proposed'),
    ('press','Angry Metal Guy','contact@angrymetalguy.com','angrymetalguy.com','Reviews and opinion. Respected, tough crowd.',false,true,'proposed'),
    ('press','Prog Magazine',NULL,'progmagazine.com','Progressive metal — strong genre fit.',false,true,'proposed'),
    ('press','Sputnikmusic',NULL,'sputnikmusic.com','Reviews, forum-based submission.',false,true,'proposed'),
    ('press','Blabbermouth','news@roadrunner.com','blabbermouth.net','Metal news.',false,true,'proposed'),

    -- Indie and underground blogs. Free, fast, and the tier to start with.
    ('press','Metal Mantra',NULL,'metal-mantra.com','Free submission with editorial review, all subgenres.',false,true,'proposed'),
    ('press','Noob Heavy','teamnoobheavy@gmail.com','noobheavy.com','Wants a full EPK, not singles.',false,true,'proposed'),
    ('press','Inhale the Heavy',NULL,'inhaletheheavy.com','Doom, sludge, black and stoner focus — weaker fit for Virya.',false,true,'proposed'),
    ('press','Metal Noise',NULL,'metalnoise.net','Album and EP reviews.',false,true,'proposed'),
    ('press','Ever Metal','contact@ever-metal.com','ever-metal.com','UK-focused indie metal; prefers UK shows.',false,true,'proposed'),
    ('press','Amplify the Noise',NULL,'amplifythenoise.com','Genre-agnostic, honest curated listening.',false,true,'proposed'),
    ('press','The Nwothm','thenwothm@gmail.com','thenwothm.com','Traditional and NWOBHM-leaning — may not fit Virya''s modern sound.',false,true,'proposed'),
    ('press','Musicngear','info+metalcore.submissions@musicngear.com','musicngear.com','Metalcore section, feature plus playlist inclusion.',false,true,'proposed'),

    -- Polish press. Local band, local press — the highest hit rate available.
    ('press','Rockserwis.pl',NULL,'rockserwis.pl','Polish rock and metal. Local-press advantage.',false,true,'proposed'),
    ('press','Teraz Rock',NULL,'terazrock.pl','Polish music magazine.',false,true,'proposed'),
    ('press','rockmetal.pl',NULL,'rockmetal.pl','Polish rock/metal news, reviews and concert announcements.',false,true,'proposed'),
    ('press','Polish Metal Promotion',NULL,'facebook.com/PolishMetalPromo','Takes Polish underground metal to an international audience.',false,true,'proposed'),

    -- Review channels. Send an EPK with streaming links, not a channel plug.
    ('creator','Pit Check',NULL,'youtube.com/@pitcheck','Weekly new metalcore and deathcore — exactly Virya''s genre.',false,true,'proposed'),
    ('creator','Metal Trenches',NULL,'youtube.com/c/metaltrenches','Discovery-focused reviews, good for underground bands.',false,true,'proposed'),
    ('creator','TwoToeTags',NULL,'youtube.com/@TwoToeTags','Metal reviews.',false,true,'proposed'),
    ('creator','theneedledrop',NULL,'youtube.com/c/theneedledrop','Broad music reviews. Very long odds, very high payoff.',false,false,'proposed'),
    ('creator','Metal Hammer (YouTube)',NULL,'youtube.com/@metalhammer','Official Metal Hammer channel.',false,false,'proposed'),

    -- TikTok. The fastest discovery surface for metal right now.
    ('creator','@metalhammeruk',NULL,'tiktok.com/@metalhammeruk','Metal Hammer magazine on TikTok.',false,false,'proposed'),
    ('creator','@metalnews',NULL,'tiktok.com/@metalnews','Metal news and recommendations, 12.2K followers.',false,false,'proposed'),
    ('creator','@mutiilati0n',NULL,'tiktok.com/@mutiilati0n','Metalhead content creator, 80.4K followers.',false,false,'proposed'),
    ('creator','@deathcoredadofficial',NULL,'tiktok.com/@deathcoredadofficial','Deathcore content, 5.7K followers.',false,false,'proposed'),
    ('creator','Sevent',NULL,'groover.co','Metal/metalcore recommendations, 132K followers. Takes submissions via Groover.',false,true,'proposed')
) AS v(target_kind, display_name, contact_email, contact_domain, why_fit, verified, accepts_outreach, status)
WHERE w.slug = current_setting('seed.slug')
ON CONFLICT (workspace_id, display_name, target_kind) DO NOTHING;

-- ---------------------------------------------------------------------------
-- The same subreddits as community outreach targets.
--
-- `discovery_places` is what the world model counts; it is not what the
-- community engager acts on. That path reads `agent_outreach_targets` where
-- `target_kind = 'community'`, `subreddit IS NOT NULL` and `status =
-- 'promoted'` — and with none of those rows existing, the brain never raised a
-- single `community.engage.request`. Zero posts had ever been made.
--
-- Only the subreddits whose own rules permit posting your own music are
-- promoted. The rest are proposed: visible, rankable, and requiring an operator
-- to say yes, because posting promo into a community that forbids it costs the
-- band its reputation there permanently.
--
-- Volume is bounded below this table regardless. The community executor allows
-- one post per subreddit per 7 days and three per workspace per 24 hours, and
-- the outreach policy caps actions at 2 per 24 hours.
INSERT INTO agent_outreach_targets
    (workspace_id, target_kind, display_name, subreddit, why_fit, verified, accepts_outreach, status)
SELECT w.id, 'community', v.display_name, v.subreddit, v.why_fit, true, true, v.status
FROM workspaces w
CROSS JOIN (VALUES
    -- Self-promotion is the point of these. Safe to act on unattended.
    ('r/ShareYourMusic','ShareYourMusic','Dedicated to self-promotion — posting directly is what it is for.','promoted'),
    ('r/ListenToThis','ListenToThis','Self-promo allowed with proper tags.','promoted'),
    ('r/findnewmetal','findnewmetal','Built for discovery — post directly with genre tags.','promoted'),
    ('r/headbangtothis','headbangtothis','Self-promo allowed. Smaller and engaged.','promoted'),
    ('r/metalvideos','metalvideos','Self-promo allowed for music videos.','promoted'),

    -- Genre fit, but promo is limited or forbidden. An operator decides.
    ('r/metalcore','metalcore','1.0M members, exact genre. Limited self-promo — recommendation threads only.','proposed'),
    ('r/progmetal','progmetal','296K members, exact genre. Recommendation threads only, no direct promo.','proposed'),
    ('r/Djent','Djent','Direct genre fit.','proposed'),
    ('r/ModernProg','ModernProg','Direct genre fit.','proposed'),
    ('r/deathcore','deathcore','190K members. Limited self-promo, newcomer-friendly threads.','proposed'),
    ('r/posthardcore','posthardcore','Limited self-promo, recommendation threads.','proposed'),
    ('r/melodicdeathmetal','melodicdeathmetal','67K members, adjacent genre.','proposed'),
    ('r/heavymind','heavymind','Thoughtful progressive metal discussion.','proposed'),
    ('r/Metal','Metal','2.6M members. No direct promo — earn trust in discussions first.','proposed'),
    ('r/Polska','Polska','Polish community. Concert announcements, never music spam.','proposed')
) AS v(display_name, subreddit, why_fit, status)
WHERE w.slug = current_setting('seed.slug')
ON CONFLICT (workspace_id, display_name, target_kind) DO NOTHING;

COMMIT;
