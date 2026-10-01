CREATE TABLE fan_capture_contexts (
 workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
 fan_id uuid NOT NULL REFERENCES fans(id) ON DELETE CASCADE,
 context jsonb NOT NULL CHECK (jsonb_typeof(context)='object'),
 created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY(workspace_id,fan_id)
);
