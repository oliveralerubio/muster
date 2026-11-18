-- Server-side association between a Paddle checkout transaction and the
-- workspace that asked for it. A `subscription.created` webhook only grants
-- rights when its `transaction_id` is one of these rows: `custom_data` set by
-- a browser checkout is never trusted.
CREATE TABLE billing_checkouts (
    transaction_id TEXT PRIMARY KEY,
    workspace_id   UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_by     UUID NOT NULL REFERENCES users(id),
    plan           TEXT NOT NULL CHECK (plan IN ('personal', 'team')),
    seats          INT NOT NULL CHECK (seats >= 1),
    price_id       TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX billing_checkouts_ws_idx ON billing_checkouts(workspace_id);

-- Durable reconciliation queue. A webhook only enqueues (in the same
-- transaction that records the event); entitlements are written from the
-- subscription returned by `GET /subscriptions/{id}`. One row per
-- subscription coalesces duplicate and out-of-order events; `generation`
-- detects events that arrive while a fetch is in flight.
CREATE TABLE billing_reconcile_jobs (
    paddle_subscription_id TEXT PRIMARY KEY,
    workspace_id           UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    generation             BIGINT NOT NULL DEFAULT 1,
    attempts               INT NOT NULL DEFAULT 0,
    next_attempt_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until           TIMESTAMPTZ,
    last_error             TEXT,
    created_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at             TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX billing_reconcile_jobs_due_idx ON billing_reconcile_jobs(next_attempt_at);

-- Canonical ordering comes from the provider's `updated_at`, not from
-- webhook `occurred_at`.
ALTER TABLE subscriptions ADD COLUMN provider_updated_at TIMESTAMPTZ;
ALTER TABLE subscriptions ALTER COLUMN last_event_occurred_at DROP NOT NULL;
