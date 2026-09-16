-- Sprint 3.5a — the content engine's units: peers, observations, the format
-- catalogue, production events, capture plans, arcs, suggestions, outcomes.
--
-- Eight tables. The catalogue (`viryaos_content_format_entries`) is global
-- seed data — a prior, not a rule (§4b-2); hypothesis.rs learns per band
-- which entries work and retires the rest, so no workspace column exists on
-- it. Everything else is workspace-scoped like every other viryaos table:
-- `workspace_id` REFERENCES workspaces(id) ON DELETE CASCADE, and children
-- carry composite (workspace_id, parent_id) FKs so a row in one workspace
-- can never point at a parent in another (the 0088 pattern).
--
-- The units, per the plan's Phase 3.5:
--   Peer              a named artist the band watches, operator-curated
--   PeerObservation   one dated fact with a link — never a summary, never a vibe
--   Arc               a campaign with a horizon and a spine of planned beats
--   ContentSuggestion concept + evidence + window + the distribution promise
--   SuggestionOutcome done / declined / done-differently, and what happened
--   ProductionEvent   a day the band generates material
--   CapturePlan       issued BEFORE the day — the harvest rule (§4b-3)
--   FormatEntry       one seeded, stable format with its effort pair

-- ── The catalogue: a prior, not a rule ────────────────────────────────────
CREATE TABLE viryaos_content_format_entries (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    key                 TEXT NOT NULL UNIQUE CHECK (btrim(key) <> ''),
    name                TEXT NOT NULL CHECK (btrim(name) <> ''),
    category            TEXT NOT NULL CHECK (category IN
        ('release', 'collaboration', 'live', 'evergreen', 'credibility')),
    purpose             TEXT NOT NULL CHECK (purpose IN
        ('acquisition', 'retention', 'conversion', 'credibility')),
    -- §4b-3: effort is marginal, not absolute. A making-of is high effort on
    -- its own day and near-zero while a shoot is already happening — so the
    -- entry carries both numbers, and the suggestion engine reads the
    -- marginal one when a production event covers it.
    effort_standalone   TEXT NOT NULL CHECK (effort_standalone IN ('low','medium','high')),
    effort_marginal     TEXT NOT NULL CHECK (effort_marginal IN ('low','medium','high')),
    -- Maps to domain TeamSkill::as_str — routes the beat to a roster member.
    skill               TEXT NOT NULL CHECK (skill IN
        ('general','operations','booking','approval','technical','visual',
         'video','photography','social','english_copy','polish_copy','people')),
    -- What the format needs to exist: a release, a show, or nothing. The
    -- 'nothing' entries are how daily cadence works without inventing a
    -- premise.
    requires            TEXT NOT NULL CHECK (requires IN ('release','show','nothing')),
    -- Which artifacts and channels the format feeds — the promise the
    -- suggestion carries, stated as the surface it can reach.
    distribution        TEXT NOT NULL,
    cadence             TEXT NOT NULL CHECK (cadence IN ('one_off','recurring','release_tied')),
    -- Where the format lands hardest. A bias, never a filter — the lateral
    -- peer tier exists to import moves a genre has not tried yet.
    genre_fit           TEXT[] NOT NULL DEFAULT '{}',
    notes               TEXT NOT NULL DEFAULT '',
    active              BOOLEAN NOT NULL DEFAULT TRUE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Peers: operator-curated, never guessed ────────────────────────────────
CREATE TABLE viryaos_peers (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name                TEXT NOT NULL CHECK (btrim(name) <> ''),
    -- {"spotify": "...", "youtube": "...", "bandcamp": "...", ...} — keys are
    -- platform names; the observation sweep reads whichever a peer uses.
    handles             JSONB NOT NULL DEFAULT '{}',
    tier                TEXT NOT NULL CHECK (tier IN ('aspirational','near_peer','lateral')),
    -- Which dimensions are worth observing for this peer — the trend
    -- dimensions (format/theme/styling/timing/platform) plus free entries
    -- like 'tour_routing'. Not a CHECK: the vocabulary grows with the
    -- observation kinds, and a stored word costs nothing.
    watch_for           TEXT[] NOT NULL DEFAULT '{}',
    -- Why they are on the list, in the operator's words. A peer without a
    -- reason is a peer nobody chose.
    why                 TEXT NOT NULL DEFAULT '',
    -- 'operator' for typed-in entries; a scanner name for proposed
    -- candidates. The system may propose; the operator confirms — a
    -- candidate is not a peer until then.
    proposed_by         TEXT NOT NULL DEFAULT 'operator',
    status              TEXT NOT NULL DEFAULT 'proposed' CHECK (status IN
        ('proposed','confirmed','rejected')),
    -- Recorded on rejection so the same wrong name is not proposed twice.
    rejection_reason    TEXT,
    confirmed_at        TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Composite-FK parent: children reference (workspace_id, peer_id) so a
    -- row can never point at a peer in another workspace.
    UNIQUE (workspace_id, id)
);
CREATE UNIQUE INDEX viryaos_peers_name_uniq
    ON viryaos_peers (workspace_id, lower(btrim(name)))
    WHERE status <> 'rejected';
CREATE INDEX viryaos_peers_active_idx
    ON viryaos_peers (workspace_id, status, tier);

-- ── Peer observations: one dated fact with a link ─────────────────────────
CREATE TABLE viryaos_peer_observations (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    peer_id             UUID NOT NULL,
    -- The date the fact happened, not when we noticed it. A trend over
    -- observations is only honest if the date is the event's own.
    observed_at         DATE NOT NULL,
    captured_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Which surface produced it — 'youtube', 'bandcamp', 'instagram', ...
    platform            TEXT NOT NULL CHECK (btrim(platform) <> ''),
    -- 'release', 'post', 'tour_announce', 'styling', ... — free text; the
    -- observation kinds grow with what the sweep learns to see.
    kind                TEXT NOT NULL CHECK (btrim(kind) <> ''),
    -- The fact itself, one sentence: "posted a playthrough, 1.2M views in
    -- 9 days". Never a summary, never a vibe.
    fact                TEXT NOT NULL CHECK (btrim(fact) <> ''),
    url                 TEXT,
    -- Views/likes/etc. as the source reported them — {"views": 1200000}.
    metrics             JSONB NOT NULL DEFAULT '{}',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, peer_id)
        REFERENCES viryaos_peers (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX viryaos_peer_observations_tail_idx
    ON viryaos_peer_observations (workspace_id, peer_id, observed_at DESC);
-- The same fact from the same platform at the same date must not be
-- recorded twice by two sweeps. Platform is part of the key: the same
-- wording on two surfaces is two observations.
CREATE UNIQUE INDEX viryaos_peer_observations_dedup_idx
    ON viryaos_peer_observations
    (workspace_id, peer_id, observed_at, platform, kind, md5(fact));

-- ── Production events: a day the band generates material ──────────────────
CREATE TABLE viryaos_production_events (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind                TEXT NOT NULL CHECK (kind IN
        ('shoot','studio','rehearsal','show','drive','photoshoot','festival','other')),
    title               TEXT NOT NULL CHECK (btrim(title) <> ''),
    -- A production day is a date; the capture plan's reminder keys off it.
    scheduled_for       DATE NOT NULL,
    -- When the event is a show, the gig it is — the T-21 ladder already
    -- knows the date and the venue. SET NULL: the production day survives
    -- the gig row's removal.
    event_id            UUID REFERENCES events(id) ON DELETE SET NULL,
    status              TEXT NOT NULL DEFAULT 'scheduled' CHECK (status IN
        ('scheduled','in_progress','done','cancelled')),
    notes               TEXT NOT NULL DEFAULT '',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);
CREATE INDEX viryaos_production_events_due_idx
    ON viryaos_production_events (workspace_id, scheduled_for)
    WHERE status = 'scheduled';

-- ── Capture plans: issued before the day, or the harvest is lost ──────────
CREATE TABLE viryaos_capture_plans (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    production_event_id UUID NOT NULL,
    -- Ordered shot list: [{"item": "10 min handheld on set", "skill": "video"}, ...]
    items               JSONB NOT NULL DEFAULT '[]',
    -- Who holds the camera — routed through select_team_assignee like any
    -- other task. Soft reference, same convention as team assignments.
    assignee_member_id  UUID,
    status              TEXT NOT NULL DEFAULT 'draft' CHECK (status IN
        ('draft','issued','done','abandoned')),
    issued_at           TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, production_event_id)
        REFERENCES viryaos_production_events (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX viryaos_capture_plans_event_idx
    ON viryaos_capture_plans (workspace_id, production_event_id);

-- ── Arcs: the unit the band approves ──────────────────────────────────────
CREATE TABLE viryaos_arcs (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    title               TEXT NOT NULL CHECK (btrim(title) <> ''),
    -- The premise in one paragraph — what this arc is for and why now.
    summary             TEXT NOT NULL DEFAULT '',
    horizon_start       DATE,
    horizon_end         DATE,
    CHECK (horizon_start IS NULL OR horizon_end IS NULL
        OR horizon_start <= horizon_end),
    -- The spine: planned beats [{"at": "2026-10-02", "beat": "playthrough",
    -- "format_key": "playthrough"}, ...]. Dates are intentions, not
    -- commitments — the suggestion engine fills each beat's concrete work.
    spine               JSONB NOT NULL DEFAULT '[]',
    -- The observations and trends behind the shape, as references —
    -- {"peer_observations": [id...], "notes": "..."}. The band approves the
    -- arc BECAUSE of this evidence, so it is stored, not paraphrased.
    evidence            JSONB NOT NULL DEFAULT '{}',
    status              TEXT NOT NULL DEFAULT 'proposed' CHECK (status IN
        ('proposed','approved','active','completed','retired')),
    approved_at         TIMESTAMPTZ,
    approved_by         TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);
CREATE INDEX viryaos_arcs_active_idx
    ON viryaos_arcs (workspace_id, status)
    WHERE status IN ('approved','active');

-- ── Suggestions: concept + evidence + the distribution promise ────────────
CREATE TABLE viryaos_content_suggestions (
    id                  UUID PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The arc this beat serves, when one exists. A suggestion outside any
    -- arc is allowed — the calendar and the peers keep producing reasons —
    -- but the field stays visible so the operator can always tell.
    arc_id              UUID,
    -- The catalogue entry the concept maps to; NULL for bespoke concepts
    -- that no seeded format covers.
    format_key          TEXT REFERENCES viryaos_content_format_entries(key),
    concept             TEXT NOT NULL CHECK (btrim(concept) <> ''),
    -- Why now, in evidence — the observations and trend behind it.
    reason              TEXT NOT NULL DEFAULT '',
    evidence            JSONB NOT NULL DEFAULT '{}',
    suggested_after     DATE,
    suggested_before    DATE,
    CHECK (suggested_after IS NULL OR suggested_before IS NULL
        OR suggested_after <= suggested_before),
    -- The marginal effort estimate once any covering production event is
    -- counted — the number a band actually reads.
    effort              TEXT CHECK (effort IN ('low','medium','high')),
    -- Who in the band would do it — proposed, routed on approval.
    proposed_assignee_member_id UUID,
    -- The assembled promise, clause by clause:
    -- {"communities": [...], "press_contacts": n, "consented_fans": n, ...}
    -- An empty promise is a suggestion not worth making — the engine
    -- declines rather than raise one.
    distribution_promise JSONB NOT NULL DEFAULT '{}',
    status              TEXT NOT NULL DEFAULT 'raised' CHECK (status IN
        ('raised','approved','declined','expired','done')),
    expires_at          TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    FOREIGN KEY (workspace_id, arc_id)
        REFERENCES viryaos_arcs (workspace_id, id) ON DELETE SET NULL
);
CREATE INDEX viryaos_content_suggestions_open_idx
    ON viryaos_content_suggestions (workspace_id, status, created_at DESC)
    WHERE status IN ('raised','approved');

-- ── Outcomes: every suggestion resolves to one ────────────────────────────
CREATE TABLE viryaos_suggestion_outcomes (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    suggestion_id       UUID NOT NULL,
    outcome             TEXT NOT NULL CHECK (outcome IN
        ('done','declined','done_differently','expired')),
    resolved_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    decided_by          TEXT,
    -- "Not for us" is a first-class signal: the decline reason suppresses
    -- the concept for this tenant. Recorded verbatim, one click.
    reason              TEXT,
    -- What happened after — reach/fans/metrics as measured:
    -- {"views": ..., "new_fans": ..., "measured_at": "..."}.
    results             JSONB NOT NULL DEFAULT '{}',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (workspace_id, suggestion_id)
        REFERENCES viryaos_content_suggestions (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX viryaos_suggestion_outcomes_suggestion_idx
    ON viryaos_suggestion_outcomes (workspace_id, suggestion_id, resolved_at DESC);

-- ── The seed: §4b-2's catalogue, verbatim ─────────────────────────────────
-- effort pairs are (standalone, marginal): near-zero marginal means "do it
-- while a production day is already happening".
INSERT INTO viryaos_content_format_entries
    (key, name, category, purpose, effort_standalone, effort_marginal, skill, requires, distribution, cadence, genre_fit, notes)
VALUES
    -- Tied to a release
    ('single_announce_presave','Single announce with pre-save','release','conversion','low','low','social','release','post → communities, consented fans, press list','release_tied','{}','The spine every release campaign hangs on'),
    ('lyric_video','Lyric video','release','acquisition','low','low','visual','release','video artifact → YouTube, communities, fans','release_tied','{}','Cheapest way to give a track a video surface'),
    ('official_video','Official video','release','acquisition','high','medium','video','release','video artifact → YouTube, press, fans, communities','release_tied','{}','The arc''s centrepiece; plan backwards from the shoot date'),
    ('playthrough','Playthrough','release','acquisition','medium','low','video','release','video artifact → YouTube, gear communities, fans','release_tied','{metal}','Metal staple, and the most reliable format in the genre'),
    ('track_by_track','Track-by-track commentary','release','retention','low','low','english_copy','release','post series → communities, fans, socials','release_tied','{}','One session yields a week of posts'),
    ('making_of','Making-of / studio diary','release','retention','medium','low','video','release','video artifact → YouTube, fans, socials','release_tied','{}','Works during the work — no extra day needed'),
    ('behind_artwork','Behind the artwork','release','retention','low','low','visual','release','post → socials, fans, styling communities','release_tied','{}','Carries the styling trend directly'),
    ('stripped_version','Stripped or acoustic version','release','retention','medium','medium','technical','release','audio+video → YouTube, streaming, fans','release_tied','{}','Second life for a song already released'),
    ('remix_edit','Remix or extended edit','release','acquisition','medium','medium','technical','release','audio → streaming, DJ pools, clubs','release_tied','{dance}','Dance-first; also a collab surface'),
    -- Collaboration — the only class that reaches somebody else''s audience
    ('guest_feature','Guest feature','collaboration','acquisition','medium','medium','people','release','track → both audiences, streaming, press','one_off','{}','Both audiences, one track'),
    ('split_release','Split release','collaboration','acquisition','medium','medium','operations','release','release → both audiences, Bandcamp, press','one_off','{metal}','Shared cost, shared reach, common in metal'),
    ('peer_cover','Cover of a peer''s song','collaboration','acquisition','low','low','video','nothing','video artifact → YouTube, the peer''s orbit','one_off','{}','Cheap, and the peer usually notices — which is the point'),
    ('remix_swap','Remix swap','collaboration','acquisition','medium','medium','technical','release','audio → both audiences, streaming','one_off','{dance}','Dance equivalent of a split'),
    ('b2b_set','B2B set','collaboration','acquisition','medium','medium','people','show','set → both audiences, live clips','one_off','{dance}','DJ-native collaboration'),
    ('compilation_sampler','Compilation or label sampler','collaboration','acquisition','low','low','operations','release','release → label audience, Bandcamp','one_off','{}','Label-level lever across a roster'),
    ('fan_cover_feature','Fan cover feature','collaboration','retention','low','low','social','nothing','post → fans, socials','recurring','{}','Costs nothing, rewards the people who already care'),
    -- Live
    ('show_announce','Tour or show announce','live','conversion','low','low','social','show','post → LiveListing, communities, fans','recurring','{}','Feeds LiveListing automatically'),
    ('live_session','Live session, one take','live','credibility','high','medium','video','nothing','video artifact → YouTube, press, fans','one_off','{}','The credibility format'),
    ('soundcheck_clip','Soundcheck or backstage clip','live','retention','low','low','video','show','clip → socials, fans','recurring','{}','Filmed on a day already spent'),
    ('aftermovie','After-movie','live','conversion','medium','low','video','show','video artifact → YouTube, ticket page, fans','recurring','{}','Sells the next show, not the last one'),
    ('tour_diary','Tour diary','live','retention','low','low','social','show','post series → socials, fans','recurring','{}','Recurring, low effort, high attachment'),
    ('listening_party','Listening party or meet-up','live','conversion','low','medium','people','release','event → attendees, fans','release_tied','{}','Converts fans into attendees'),
    -- Evergreen — requires nothing
    ('gear_rundown','Gear rundown','evergreen','acquisition','low','low','video','nothing','video artifact → YouTube, gear communities','recurring','{metal,dance}','Reliably over-performs in metal and dance'),
    ('influences_list','Influences / what we are listening to','evergreen','retention','low','low','social','nothing','post → socials, communities','recurring','{}','Also a peer-discovery signal in itself'),
    ('qa_ama','Q&A or AMA','evergreen','retention','low','low','people','nothing','session → admitted communities, socials','recurring','{}','Works in communities where the band is admitted'),
    ('rehearsal_clip','Rehearsal clip','evergreen','retention','low','low','video','nothing','clip → socials, fans','recurring','{}','Filmed during work already happening'),
    ('old_material_reaction','Reaction to own older material','evergreen','retention','low','low','video','nothing','video artifact → socials, fans','recurring','{}','Anniversary-friendly'),
    ('merch_drop','Merch drop or restock','evergreen','conversion','low','low','visual','nothing','post → fans, socials, shop','recurring','{}','Conversion, not reach'),
    ('release_anniversary','Release anniversary','evergreen','retention','low','low','social','nothing','post → socials, fans, streaming resurface','recurring','{}','The calendar supplies the trigger'),
    ('fan_content_feature','Fan content feature','evergreen','retention','low','low','social','nothing','post → fans, socials','recurring','{}','Tattoos, covers, photos from the pit'),
    -- Credibility
    ('interview_podcast','Interview or podcast','credibility','credibility','medium','medium','people','nothing','episode → podcast audience, press','recurring','{}','Phase 3 finds the shows'),
    ('press_feature','Press feature','credibility','credibility','medium','medium','english_copy','nothing','feature → outlet audience, press list','recurring','{}','Phase 3 finds the writers'),
    ('playlist_curation','Playlist curation','credibility','credibility','low','low','social','nothing','playlist → streaming, peer orbit','recurring','{}','Positions the band among its peers'),
    ('cause_tie_in','Cause or charity tie-in','credibility','credibility','medium','medium','people','nothing','campaign → press, communities','one_off','{}','Only when genuine; fake ones are read instantly');
