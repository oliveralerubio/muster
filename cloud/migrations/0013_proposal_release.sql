-- A closed proposal (rejected, resolved, superseded) gives its retained
-- upload back: the maintenance worker deletes its `refs/muster/uploads/<id>`
-- ref, drops the objects nothing else reaches, and records when here. Open
-- proposals and every accepted upload are never released.
ALTER TABLE proposals ADD COLUMN bundle_released_at TIMESTAMPTZ;
CREATE INDEX proposals_release_due_idx ON proposals(workspace_id)
    WHERE has_bundle AND bundle_released_at IS NULL AND status <> 'open';
