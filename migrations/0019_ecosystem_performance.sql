-- Targeted indexes for reconciliation JSON lookups. These are partial, so they
-- do not tax unrelated outbox writes or duplicate the worker claim indexes.

CREATE INDEX IF NOT EXISTS outbox_events_ticket_paid_order_lookup_idx
    ON outbox_events (workspace_id, ((payload ->> 'order_id')))
    WHERE event_type = 'ticket.order.paid';

CREATE INDEX IF NOT EXISTS outbox_events_ticket_delivery_order_lookup_idx
    ON outbox_events (workspace_id, ((payload ->> 'order_id')), created_at)
    WHERE event_type = 'ticket.order.delivery_requested';
