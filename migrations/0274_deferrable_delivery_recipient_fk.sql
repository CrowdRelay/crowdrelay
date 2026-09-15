-- The delivery ledger's recipient FK is checked per-statement. A fan merge
-- re-points both tables inside one transaction, which no statement order can
-- satisfy: updating the parent orphans the child, and updating the child
-- first leaves it pointing at a parent that does not exist yet. Recreating
-- the constraint DEFERRABLE keeps the default immediate check for every
-- existing writer while letting the merge transaction defer it until commit.
ALTER TABLE communication_campaign_deliveries
    DROP CONSTRAINT communication_campaign_deliveries_recipient_fk,
    ADD CONSTRAINT communication_campaign_deliveries_recipient_fk
        FOREIGN KEY (workspace_id, campaign_id, fan_id)
        REFERENCES communication_campaign_recipients (workspace_id, campaign_id, fan_id)
        ON DELETE CASCADE
        DEFERRABLE INITIALLY IMMEDIATE;
