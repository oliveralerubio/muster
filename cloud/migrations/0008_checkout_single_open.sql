-- At most one open checkout per workspace, so retries and concurrent
-- requests reuse it instead of creating more Paddle transactions (each
-- payable on its own: double charge).
--   open       offered to the customer (`checkout_url`)
--   canceled   canceled at Paddle (PATCH /transactions/{id} status=canceled)
--   completed  paid: Paddle created a subscription from it
--   superseded no longer offered by this server (rows from before this
--              migration, which have no stored URL)
ALTER TABLE billing_checkouts
    ADD COLUMN status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'canceled', 'completed', 'superseded')),
    ADD COLUMN checkout_url TEXT,
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();

UPDATE billing_checkouts SET status = 'superseded' WHERE checkout_url IS NULL;

CREATE UNIQUE INDEX billing_checkouts_one_open
    ON billing_checkouts(workspace_id) WHERE status = 'open';
