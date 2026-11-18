-- Invitations to workspaces.
-- Token is a 256-bit random string; only its SHA-256 hash is persisted.
CREATE TABLE invitations (
    id            UUID PRIMARY KEY,
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    email         TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin', 'member')),
    token_hash    BYTEA NOT NULL UNIQUE,
    invited_by    UUID NOT NULL REFERENCES users(id),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    accepted_at   TIMESTAMPTZ,
    accepted_by   UUID REFERENCES users(id)
);
CREATE INDEX invitations_ws_idx ON invitations(workspace_id, created_at DESC);
