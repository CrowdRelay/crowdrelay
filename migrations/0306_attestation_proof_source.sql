-- Attestations join the transparency ledger's source kinds (Sprint 4A.6).
--
-- An attestation is a document a buyer acts on, which is exactly the reason
-- audit events and operator actions are anchored: somebody outside this
-- platform makes a decision on it. Until now the only proof a presented
-- attestation was ours was its HMAC signature, which asks the reader to trust
-- CrowdRelay's key. Including the attestation in `external_proof_items`
-- replaces that ask with a checkable fact — the batch root commits to the
-- document's digest at a point in time, and the Merkle path to it is
-- fetchable through the public inclusion route with no credential at all.
--
-- Widening a CHECK revalidates existing rows rather than rewriting them, and
-- no existing row can fail this one: the permitted set only grew. The
-- constraint was created inline in 0020_external_proofs.sql, so PostgreSQL
-- named it `external_proof_items_source_kind_check`.
ALTER TABLE external_proof_items
    DROP CONSTRAINT external_proof_items_source_kind_check;
ALTER TABLE external_proof_items
    ADD CONSTRAINT external_proof_items_source_kind_check
    CHECK (source_kind IN ('audit_event', 'operator_action', 'reward_draw_run', 'attestation'));
