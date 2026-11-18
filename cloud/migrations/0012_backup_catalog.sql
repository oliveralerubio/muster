-- Projection of the managed backup root (cloud/src/backup.rs). The root is
-- the truth; every `backup run` / `retention run` re-synchronises these rows,
-- so a restored database catches up on the next run.
CREATE TABLE backup_snapshots (
    backup_id          TEXT PRIMARY KEY,
    snapshot_at        TIMESTAMPTZ NOT NULL,
    normal_expires_at  TIMESTAMPTZ NOT NULL,
    tenants            UUID[] NOT NULL,
    removed_at         TIMESTAMPTZ
);
CREATE INDEX backup_snapshots_tenants ON backup_snapshots USING gin (tenants);

-- One row: the last inventory of the backup root.
CREATE TABLE backup_inventory (
    id                  SMALLINT PRIMARY KEY CHECK (id = 1),
    inventoried_at      TIMESTAMPTZ NOT NULL,
    uncatalogued        INTEGER NOT NULL,
    overdue_grace_secs  BIGINT NOT NULL,
    last_error          TEXT,
    last_retention_at   TIMESTAMPTZ
);
