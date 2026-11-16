-- Primary purge of a workspace with a deletion request (`crate::lifecycle_purge`).
-- `purge_committed_at` / `purged_at` project the authority's irreversible
-- `workspace_tombstoned` / `workspace_purged` records (rebuilt by replay into
-- any restored database); the other columns are operational bookkeeping of
-- the purge worker, never a source of truth.
ALTER TABLE workspace_lifecycle
    ADD COLUMN purge_committed_at    TIMESTAMPTZ,
    ADD COLUMN purged_at             TIMESTAMPTZ,
    ADD COLUMN purge_attempts        INT NOT NULL DEFAULT 0,
    ADD COLUMN purge_last_error      TEXT,
    ADD COLUMN purge_next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now();

-- Every provider object id the purged workspace ever had (subscriptions of
-- any status, checkout transactions), captured just before its rows are
-- erased. No foreign key: it outlives the tenant rows so that billing events
-- about those objects, stored or arriving later, keep no payload. Ids only.
CREATE TABLE lifecycle_purged_provider_ids (
    workspace_id UUID NOT NULL,
    provider_id  TEXT NOT NULL,
    PRIMARY KEY (workspace_id, provider_id)
);
CREATE INDEX lifecycle_purged_provider_ids_by_id ON lifecycle_purged_provider_ids (provider_id);
