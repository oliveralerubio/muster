-- The maintenance worker deletes bookkeeping past its retention (replay
-- nonces, idempotency records, expired credentials and logins) in small
-- batches; these indexes keep each batch from scanning whole tables.
CREATE INDEX device_nonces_created_idx ON device_nonces(created_at);
CREATE INDEX sync_idempotency_created_idx ON sync_idempotency(created_at);
CREATE INDEX account_idempotency_created_idx ON account_idempotency(created_at);
CREATE INDEX account_tokens_expires_idx ON account_tokens(expires_at);
CREATE INDEX sessions_expires_idx ON sessions(expires_at);
CREATE INDEX oidc_logins_expires_idx ON oidc_logins(expires_at);
CREATE INDEX device_authorizations_expires_idx ON device_authorizations(expires_at);
