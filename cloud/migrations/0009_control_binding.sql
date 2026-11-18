-- Binding of this database to the lifecycle authority (`crate::control`):
-- which deployment and control store it belongs to, its own incarnation,
-- and how far it has projected the control journal. Exactly one row once
-- bound (by `muster-cloud-ops init` or `register`); never created by the
-- service itself. A database restored from a dump carries the row of its
-- source, which the authority checks (deployment, store, sequence, hash).
CREATE TABLE control_binding (
    singleton        BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    deployment_id    UUID NOT NULL,
    control_store_id UUID NOT NULL,
    incarnation_id   UUID NOT NULL,
    applied_seq      BIGINT NOT NULL CHECK (applied_seq >= 1),
    applied_hash     TEXT NOT NULL,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
