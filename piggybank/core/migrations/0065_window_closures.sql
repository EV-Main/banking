-- 0065: the record of the share-ledger accounts the migration window closed
-- (`window-tool close-share-accounts`, org restructure plan §6.5 step 5). Expand only: one
-- new table, nothing existing is touched.
--
-- TigerBeetle accounts are immutable, so "closing" a unit holding means moving its balance
-- off (against the supply) and writing down here that the account is spent: which account,
-- how many units left it, and the ledger transfer that took them. The account's code (60,
-- 62 or 64) is thereby marked retired as far as the window is concerned; flipping
-- `AccountCode::is_retired` for those codes belongs to the release that removes the share
-- ledger from banking, after invest has taken the units over.
--
-- The row is also the window's anchor memory: the units a holder had on a closed account
-- are `units` here, checked against the transfer on the ledger by `transfer_id`, so the
-- `service_arb` anchor still reads in banking after the live balances are zero.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS window_closures;
--   Safe at any time before the first close ran. After it, the rows are the only
--   Postgres-side record of which accounts were emptied by the window; the transfers stay
--   on the ledger either way. Rolling the image back past this migration also needs, as
--   `postgres`, BEFORE the older image starts: DELETE FROM _sqlx_migrations WHERE version = 65;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version.
SET LOCAL lock_timeout = '3s';

CREATE TABLE IF NOT EXISTS window_closures (
    logical_key TEXT        PRIMARY KEY,
    service     TEXT        NOT NULL,
    kind        TEXT        NOT NULL CHECK (kind IN ('user_shares', 'book_shares', 'fee_shares')),
    -- The holder; NULL exactly for a product's fee class, which the `fee` allocation holds.
    user_id     UUID,
    -- The closed account's TigerBeetle code (60, 62 or 64): the code being retired.
    code        INTEGER     NOT NULL CHECK (code IN (60, 62, 64)),
    units       TEXT        NOT NULL CHECK (units ~ '^[1-9][0-9]*$'),
    -- The ledger transfer that moved the units off, as 16 big-endian bytes like `tb_accounts`.
    transfer_id BYTEA       NOT NULL UNIQUE CHECK (octet_length(transfer_id) = 16),
    closed_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((kind = 'fee_shares') = (user_id IS NULL))
);
CREATE INDEX IF NOT EXISTS window_closures_service_idx ON window_closures (service);
