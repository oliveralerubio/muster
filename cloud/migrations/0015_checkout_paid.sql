-- A checkout Paddle reports paid whose subscription is not registered here
-- yet: it must keep refusing a second checkout (a double charge) until the
-- canonical reconciliation of its subscription settles it.
--   paid       paid at Paddle; `subscription_id` (once Paddle names it) is
--              being reconciled; no other checkout is offered meanwhile
--   completed  its subscription was reconciled (granted or refused)
-- `unverified_offers` counts the consecutive offers of the stored link made
-- while Paddle's status of it could not be read.
ALTER TABLE billing_checkouts DROP CONSTRAINT billing_checkouts_status_check;
ALTER TABLE billing_checkouts
    ADD CONSTRAINT billing_checkouts_status_check
        CHECK (status IN ('open', 'paid', 'canceled', 'completed', 'superseded')),
    ADD COLUMN subscription_id TEXT,
    ADD COLUMN unverified_offers INT NOT NULL DEFAULT 0;

CREATE INDEX billing_checkouts_paid_idx
    ON billing_checkouts(workspace_id) WHERE status = 'paid';
CREATE INDEX billing_checkouts_subscription_idx
    ON billing_checkouts(subscription_id) WHERE subscription_id IS NOT NULL;
