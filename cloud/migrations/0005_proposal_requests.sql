-- Proposals that are requests for local signing (enrollment, restore) and
-- their resolution. The service never applies a proposal: an authorized
-- client signs locally and publishes with POST /sync, then resolves it.
ALTER TABLE proposals DROP CONSTRAINT proposals_kind_check;
ALTER TABLE proposals ADD CONSTRAINT proposals_kind_check
    CHECK (kind IN ('conflict', 'pending_quorum', 'unauthorized', 'enrollment', 'restore'));
ALTER TABLE proposals DROP CONSTRAINT proposals_status_check;
ALTER TABLE proposals ADD CONSTRAINT proposals_status_check
    CHECK (status IN ('open', 'resolved', 'rejected', 'superseded'));
-- Requests made by an account (console restore) have no device.
ALTER TABLE proposals ALTER COLUMN device_id DROP NOT NULL;
ALTER TABLE proposals ADD COLUMN created_by UUID REFERENCES users(id);
ALTER TABLE proposals ADD COLUMN payload JSONB;
ALTER TABLE proposals ADD COLUMN has_bundle BOOLEAN NOT NULL DEFAULT true;
ALTER TABLE proposals ADD COLUMN result_revision TEXT;
ALTER TABLE proposals ADD COLUMN resolved_at TIMESTAMPTZ;
ALTER TABLE proposals ADD COLUMN resolved_by_device UUID REFERENCES devices(id) ON DELETE SET NULL;
ALTER TABLE proposals ADD COLUMN resolved_by_user UUID REFERENCES users(id);
-- At most one open enrollment request per device.
CREATE UNIQUE INDEX proposals_one_open_enrollment ON proposals(device_id)
    WHERE kind = 'enrollment' AND status = 'open';
