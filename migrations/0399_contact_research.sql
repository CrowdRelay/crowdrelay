-- What the band knows about a person before it writes to them.
--
-- The rule: nobody is written to as a stranger. Before a letter goes to a
-- promoter, a journalist, a presenter or a photographer, the band has looked at
-- what they have done lately — an episode, a review, a project — and the letter
-- opens with that. A letter that shows the sender read the work is answered far
-- more often than one that could have gone to anybody, and it is the difference
-- between a colleague writing and a mailshot.
--
-- One row is one dated, sourced thing a person did. It is keyed by the address
-- rather than by a beacon or an outreach target because the same person wears
-- both roles (`latarnik`) and the research must not be done, paid for or lost
-- twice. It records who found it: the research agent, or a person.
--
-- `observed_on` is when the thing happened or was published, not when it was
-- found, so "recent" means something: the gate refuses a fact older than the
-- window in `crowdrelay_domain::contact_research`. `source_url` is mandatory
-- and https: a fact with no source cannot be checked by the person who reads
-- the letter before it goes, and cannot be revoked if the source was wrong.
-- The letter never prints the URL (every URL in a letter must be a tracked
-- link); the operator sees it in the preview.
--
-- Append-only by use: a better fact is a new row, and the newest recent row
-- wins. Nothing here is a consent record.

CREATE TABLE contact_research (
    id               uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id     uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    normalized_email text        NOT NULL,
    fact             text        NOT NULL,
    praise           text,
    source_url       text        NOT NULL,
    observed_on      date        NOT NULL,
    language         text        NOT NULL DEFAULT 'pl',
    researched_by    text        NOT NULL,
    researched_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT contact_research_email_check
        CHECK (normalized_email = lower(btrim(normalized_email))
               AND char_length(normalized_email) BETWEEN 3 AND 320),
    CONSTRAINT contact_research_fact_check
        CHECK (btrim(fact) <> '' AND char_length(fact) <= 400),
    CONSTRAINT contact_research_praise_check
        CHECK (praise IS NULL OR (btrim(praise) <> '' AND char_length(praise) <= 300)),
    CONSTRAINT contact_research_source_check
        CHECK (source_url ~* '^https://' AND char_length(source_url) <= 2048),
    CONSTRAINT contact_research_language_check
        CHECK (language ~ '^[a-z]{2}$'),
    CONSTRAINT contact_research_by_check
        CHECK (btrim(researched_by) <> '' AND char_length(researched_by) <= 120),
    CONSTRAINT contact_research_one_per_source
        UNIQUE (workspace_id, normalized_email, source_url)
);

-- The gate's read: the newest recent fact about one address.
CREATE INDEX contact_research_lookup_idx
    ON contact_research (workspace_id, normalized_email, observed_on DESC);
