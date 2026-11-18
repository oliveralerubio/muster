-- Tramo 1: accounts, browser sessions and workspace tenancy.
-- Later tramos (devices, sync, proposals, receipts, billing) add their own
-- migrations; nothing here anticipates their shape.

-- Accounts. Identity is the OIDC (issuer, subject) pair; email is a mutable
-- display attribute and deliberately not unique.
CREATE TABLE users (
    id          UUID PRIMARY KEY,
    issuer      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    email       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (issuer, subject)
);

-- In-flight OIDC logins. Keyed by SHA-256 of `state`; bound to the browser
-- that started it by SHA-256 of the login cookie. Deleted on first use.
CREATE TABLE oidc_logins (
    state_hash     BYTEA PRIMARY KEY,
    browser_hash   BYTEA NOT NULL,
    pkce_verifier  TEXT NOT NULL,
    nonce          TEXT NOT NULL,
    return_to      TEXT NOT NULL,
    expires_at     TIMESTAMPTZ NOT NULL
);

-- Browser sessions. Only the SHA-256 of the cookie value is stored.
CREATE TABLE sessions (
    token_hash  BYTEA PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_user_idx ON sessions(user_id);

CREATE TABLE workspaces (
    id             UUID PRIMARY KEY,
    name           TEXT NOT NULL,
    created_by     UUID NOT NULL REFERENCES users(id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    trial_ends_at  TIMESTAMPTZ NOT NULL
);

CREATE TABLE memberships (
    workspace_id  UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id       UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role          TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX memberships_user_idx ON memberships(user_id);

-- Idempotency for account-scoped mutations (e.g. workspace creation): the
-- key is scoped to the authenticated user and bound to method, path and
-- body hash; a different request under the same key is rejected.
CREATE TABLE account_idempotency (
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    idem_key        UUID NOT NULL,
    method          TEXT NOT NULL,
    path            TEXT NOT NULL,
    body_sha256     BYTEA NOT NULL,
    status          INT NOT NULL,
    response        JSONB NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, idem_key)
);
