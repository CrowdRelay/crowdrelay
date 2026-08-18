-- Align existing Bandsintown-derived event URLs with PublicEvent::validate().
--
-- Older workers admitted plain HTTP and other URL forms that the public domain
-- rejects. Provider links are optional, so remove only incompatible provider
-- URLs instead of allowing one row to poison the whole public event feed.
UPDATE events
SET ticket_url = NULL
WHERE source_provider = 'bandsintown'
  AND ticket_url IS NOT NULL
  AND (
      ticket_url !~ '^https://'
      OR ticket_url ~ '^https://[^/]*@'
      OR ticket_url LIKE '%#%'
      OR ticket_url ~ '[[:space:][:cntrl:]]'
  );

UPDATE events
SET external_event_url = NULL
WHERE source_provider = 'bandsintown'
  AND external_event_url IS NOT NULL
  AND (
      external_event_url !~ '^https://'
      OR external_event_url ~ '^https://[^/]*@'
      OR external_event_url LIKE '%#%'
      OR external_event_url ~ '[[:space:][:cntrl:]]'
  );
