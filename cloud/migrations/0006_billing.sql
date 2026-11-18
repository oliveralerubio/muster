-- Subscriptions and billing events (Paddle Billing).
-- Unconfigured billing uses the trial recorded in `workspaces`.

CREATE TABLE billing_customers (
    workspace_id  UUID PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    customer_id   TEXT NOT NULL,
    email         TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE subscriptions (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id           UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    paddle_subscription_id TEXT NOT NULL UNIQUE,
    customer_id            TEXT NOT NULL,
    plan                   TEXT NOT NULL CHECK (plan IN ('personal', 'team')),
    seats                  INT NOT NULL DEFAULT 1,
    status                 TEXT NOT NULL CHECK (status IN ('active', 'past_due', 'paused', 'canceled', 'trialing')),
    current_period_start   TIMESTAMPTZ NOT NULL,
    current_period_end     TIMESTAMPTZ NOT NULL,
    cancel_at_period_end   BOOLEAN NOT NULL DEFAULT false,
    canceled_at            TIMESTAMPTZ,
    last_event_occurred_at TIMESTAMPTZ NOT NULL,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX subscriptions_ws_idx ON subscriptions(workspace_id);

CREATE TABLE billing_events (
    event_id      TEXT PRIMARY KEY,
    event_type    TEXT NOT NULL,
    workspace_id  UUID REFERENCES workspaces(id) ON DELETE SET NULL,
    occurred_at   TIMESTAMPTZ NOT NULL,
    payload       JSONB NOT NULL,
    processed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX billing_events_ws_idx ON billing_events(workspace_id, occurred_at DESC);

-- Workspace lifecycle: scheduled deletion with 30-day export window and 35-day backup expiration
ALTER TABLE workspaces ADD COLUMN deleted_at TIMESTAMPTZ;
ALTER TABLE workspaces ADD COLUMN export_until TIMESTAMPTZ;
ALTER TABLE workspaces ADD COLUMN primary_purge_at TIMESTAMPTZ;
ALTER TABLE workspaces ADD COLUMN backups_expire_at TIMESTAMPTZ;
