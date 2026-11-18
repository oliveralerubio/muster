-- Every accepted head, in acceptance order. The Git objects between two
-- accepted heads are kept (upload refs are never deleted), so the export
-- carries the complete history.
CREATE TABLE accepted_revisions (
    seq              BIGSERIAL PRIMARY KEY,
    workspace_id     UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    revision         TEXT NOT NULL,
    parent_revision  TEXT,
    epoch            BIGINT NOT NULL,
    signer           TEXT NOT NULL,
    device_id        UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    message          TEXT NOT NULL,
    accepted_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, revision)
);
CREATE INDEX accepted_revisions_ws_idx ON accepted_revisions(workspace_id, seq DESC);
