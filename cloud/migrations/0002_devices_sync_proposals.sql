-- Tramo 2: device flow (RFC 8628), account tokens, machine links, sync, proposals and receipts.

CREATE TABLE device_authorizations (
    device_code_hash  BYTEA PRIMARY KEY,
    user_code         TEXT NOT NULL UNIQUE,
    client_name       TEXT NOT NULL,
    user_id           UUID REFERENCES users(id) ON DELETE CASCADE,
    status            TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'denied')),
    interval_secs     INT NOT NULL DEFAULT 5,
    last_polled_at    TIMESTAMPTZ,
    expires_at        TIMESTAMPTZ NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX device_auth_user_code_idx ON device_authorizations(user_code);

CREATE TABLE account_tokens (
    token_hash   BYTEA PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_name  TEXT NOT NULL,
    scopes       TEXT[] NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX account_tokens_user_idx ON account_tokens(user_id);

CREATE TABLE devices (
    id            UUID PRIMARY KEY,
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    machine_name  TEXT NOT NULL,
    signing_key   TEXT NOT NULL,
    recipient     TEXT NOT NULL,
    token_hash    BYTEA NOT NULL UNIQUE,
    status        TEXT NOT NULL CHECK (status IN ('linked', 'pending', 'approved', 'revoked')),
    bootstrap     BOOLEAN NOT NULL DEFAULT false,
    linked_by     UUID NOT NULL REFERENCES users(id),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at  TIMESTAMPTZ,
    UNIQUE (workspace_id, machine_name),
    UNIQUE (workspace_id, signing_key)
);
CREATE INDEX devices_workspace_idx ON devices(workspace_id);
-- At most one machine per workspace holds the bootstrap claim.
CREATE UNIQUE INDEX devices_one_bootstrap ON devices(workspace_id) WHERE bootstrap;

CREATE TABLE device_nonces (
    device_id   UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    nonce       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, nonce)
);

CREATE TABLE workspace_heads (
    workspace_id   UUID PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    head_revision  TEXT NOT NULL,
    epoch          BIGINT NOT NULL,
    payload_hash   TEXT NOT NULL,
    anchor         JSONB NOT NULL,
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sync_idempotency (
    workspace_id   UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    device_id      UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    idem_key       UUID NOT NULL,
    method         TEXT NOT NULL,
    path           TEXT NOT NULL,
    base_revision  TEXT NOT NULL,
    body_sha256    BYTEA NOT NULL,
    status         INT NOT NULL,
    headers        JSONB NOT NULL,
    response       JSONB NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, device_id, idem_key)
);

CREATE TABLE proposals (
    id             UUID PRIMARY KEY,
    workspace_id   UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    device_id      UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL CHECK (kind IN ('conflict', 'pending_quorum', 'unauthorized')),
    status         TEXT NOT NULL CHECK (status IN ('open')),
    base_revision  TEXT NOT NULL,
    revision       TEXT NOT NULL,
    epoch          BIGINT,
    -- The bundle lives in Git under refs/muster/uploads/<id> (never deleted:
    -- anti-GC); the proposal id is that upload id.
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX proposals_ws_idx ON proposals(workspace_id, created_at DESC);

CREATE TABLE receipts (
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    device_id     UUID NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    sequence      BIGINT NOT NULL,
    revision      TEXT NOT NULL,
    epoch         BIGINT NOT NULL,
    outcome       TEXT NOT NULL CHECK (outcome IN ('applied', 'failed')),
    applied_at    TIMESTAMPTZ NOT NULL,
    received_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    signature     TEXT NOT NULL,
    PRIMARY KEY (workspace_id, device_id, sequence)
);
CREATE INDEX receipts_device_idx ON receipts(device_id, sequence DESC);
