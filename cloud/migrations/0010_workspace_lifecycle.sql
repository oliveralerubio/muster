-- Projection of the lifecycle authority's deletion records
-- (`crate::control`, `workspace_deletion_requested` and
-- `deletion_provider_item`). The journal outside the database is the source
-- of truth: these rows are rebuilt by replay into any restored, older
-- database. No foreign key to `workspaces` on purpose: the request and the
-- provider identifiers to cancel must outlive the tenant rows (purge,
-- cascades) and exist even in a dump taken before the workspace was
-- created.
CREATE TABLE workspace_lifecycle (
    workspace_id     UUID PRIMARY KEY,
    request_id       UUID NOT NULL UNIQUE,
    requested_by     UUID NOT NULL,
    requested_at     TIMESTAMPTZ NOT NULL,
    primary_purge_at TIMESTAMPTZ NOT NULL,
    backup_cap_at    TIMESTAMPTZ NOT NULL,
    journal_seq      BIGINT NOT NULL
);

-- Provider objects (Paddle) that must be canceled immediately because of a
-- deletion request: subscriptions and open checkout transactions. Only a
-- canonical read from the provider confirms an item; every failure leaves
-- it pending with its error and a retry time.
CREATE TABLE lifecycle_provider_items (
    item             TEXT NOT NULL CHECK (item IN ('subscription', 'checkout')),
    provider_id      TEXT NOT NULL,
    workspace_id     UUID NOT NULL,
    request_id       UUID NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'confirmed')),
    provider_status  TEXT,
    attempts         INT NOT NULL DEFAULT 0,
    last_error       TEXT,
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until     TIMESTAMPTZ,
    confirmed_at     TIMESTAMPTZ,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (item, provider_id)
);
CREATE INDEX lifecycle_provider_items_ws_idx ON lifecycle_provider_items(workspace_id);
CREATE INDEX lifecycle_provider_items_due_idx
    ON lifecycle_provider_items(next_attempt_at) WHERE status = 'pending';
