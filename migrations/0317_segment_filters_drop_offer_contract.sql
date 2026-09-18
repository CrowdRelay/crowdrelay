-- Remove `offer_contract` from stored audience segment filters.
--
-- `AudienceFilter` on the read side denies unknown fields. The show-growth
-- merch lever wrote an `offer_contract` object into the segment's `filter`
-- column — a prompt contract, not a predicate — so every preview of a merch
-- segment failed to deserialize and answered 503 with nothing in the log. The
-- console rendered that as "the audience backend may not support this
-- segment", which was never the problem: the backend stored the segment and
-- could not read it back.
--
-- The writer now puts those words in the campaign's `content`, where the rest
-- of that file already keeps what a writer must obey. This clears the rows
-- that were written before the fix; without it, every merch segment already on
-- disk stays unreadable for ever, because the segment is only rewritten when
-- its lever fires again.
--
-- Scoped to the one key by name rather than filtering to known fields
-- wholesale: a second unknown key would be a second bug, and silently
-- discarding it here would hide it instead of surfacing it.
UPDATE audience_segments
SET filter = filter - 'offer_contract'
WHERE filter ? 'offer_contract';
