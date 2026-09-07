-- Why a webhook attempt failed, not just that it did.
--
-- `webhook_delivery_attempts` recorded the status code and an error kind and
-- threw the response body away, so a dead letter reading "HTTP 422" gave the
-- operator nothing to act on: the receiver's own explanation — n8n answers
-- `{"accepted":false,"error":"..."}` — was read and discarded. Four event
-- types have never been accepted by the tenant's automation endpoint and the
-- console could not say why.
--
-- Bounded on write by the worker. It is an excerpt for a human reading one
-- row, not a payload store.
ALTER TABLE webhook_delivery_attempts
    ADD COLUMN IF NOT EXISTS response_excerpt text;

COMMENT ON COLUMN webhook_delivery_attempts.response_excerpt IS
    'First bytes of the receiver response body for a failed attempt, truncated by the worker. NULL when the attempt succeeded or the body was empty.';
