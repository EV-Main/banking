-- 0068: the withdrawal address book and the hold on a first-seen destination (wallet hardening
-- PR0, WALLET-INPAGE-SPEC control (e)). Expand only: one nullable column, one new table, and
-- a backfill of the destinations people have already been paid to.
--
-- A destination the user has never withdrawn to is held for `WITHDRAWAL_NEW_ADDRESS_HOLD_SECS`
-- (6 h by default): the withdrawal is accepted and stays `queued` (so the user can cancel it),
-- and the dispatcher sends nothing before `withdrawals.release_at`. The book remembers when
-- each destination was first seen and when it is released.
--
-- The backfill treats a destination with a COMPLETED withdrawal as released at the time of its
-- first one, so no existing user finds their usual address suddenly held after the deploy.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS withdrawal_addresses;
--   ALTER TABLE withdrawals DROP COLUMN IF EXISTS release_at;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 68;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   objects may stay instead of being dropped: the older binary never reads them (it also
--   no longer holds anything, so a withdrawal still `queued` with a future `release_at`
--   would dispatch early — drain or cancel those first).
SET LOCAL lock_timeout = '3s';

ALTER TABLE withdrawals ADD COLUMN IF NOT EXISTS release_at TIMESTAMPTZ;

CREATE TABLE IF NOT EXISTS withdrawal_addresses (
    user_id       UUID        NOT NULL REFERENCES users (id),
    network       TEXT        NOT NULL,
    address       TEXT        NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    release_at    TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (user_id, network, address)
);

INSERT INTO withdrawal_addresses (user_id, network, address, first_seen_at, release_at)
SELECT user_id, network, address, MIN(created_at), MIN(created_at)
FROM withdrawals
WHERE source = 'user' AND state = 'completed'
GROUP BY user_id, network, address
ON CONFLICT DO NOTHING;
