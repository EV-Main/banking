-- 0063: an index on the user of a payout line (`PayoutReviewService.ListPayoutLines`,
-- banking#529). The page that shows an approver a pending payout's lines marks each recipient
-- who was never paid in the scope before, which asks "any line of this user in a payout of
-- this scope" once per recipient; the table's only keys lead with `payout_id`, so without
-- this each of those is a scan. Expand only: one index, no column, no constraint.
--
-- A plain CREATE INDEX holds a write lock on `scope_payout_lines` while it builds. The table
-- is weeks old and holds one row per paid recipient, so the build is milliseconds; the
-- `lock_timeout` below makes the migration fail fast, not queue behind a long writer.
--
-- REVERSIBILITY: DROP INDEX IF EXISTS scope_payout_lines_user_idx;
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 63;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   index may stay instead of being dropped: the older binary never reads it.
SET LOCAL lock_timeout = '3s';

CREATE INDEX IF NOT EXISTS scope_payout_lines_user_idx ON scope_payout_lines (user_id);
