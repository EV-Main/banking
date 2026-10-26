-- 0071: banking holds no units — the CONTRACT half of the share ledger's removal (v2.21, #569).
--
-- v2.21's code reads no allocation, fund, fee, book, subscription, redemption, issuance or
-- window table, and has no `service:` party. Every row in a live table must parse with its
-- domain types (`tb_accounts` excepted: the code skips retired codes), so what only the
-- removed path understands moves to schema `retired`, unchanged:
--   * whole tables: allocations, allocation_access_grants, book_*, fee_*, fund_positions,
--     fund_valuations, redemptions, subscriptions, unit_issuances, window_closures;
--   * payments naming a `service` party on a live seat, with their concierge / session seats
--     and notices (the ones an old seat decided leave in 0072, with the seat tables);
--   * the seed's `deposits` (party_kind `piggybank`), which makes `deposits_party_is_a_user`
--     VALID;
--   * `event_log` / `outbox` rows of the unit kinds, of those payments, `balance` events that
--     are not a deposit onto a person (the seed, the retired fund claim), and withdrawal
--     settles whose fee went anywhere but the treasury (before D17: the `fee` allocation, or
--     the `fee` singleton when the payload names no payee) — v2.21 would read those as paid
--     to the treasury.
--
-- PREFLIGHT. Refuses while a removed path still has live state, and runs 0072's governance
-- checks too, so a deploy that would fail there fails before anything changes. Every
-- `SELECT count(*)` below is safe to run read-only on production before deploying.
--
-- REVERSIBILITY: after 0072's rollback, as `postgres`, statement for statement
-- (`tests/contract_migrations.rs` runs it):
--   ALTER TABLE deposits DROP CONSTRAINT deposits_party_is_a_user;
--   INSERT INTO deposits SELECT * FROM retired.deposits;
--   ALTER TABLE deposits ADD CONSTRAINT deposits_party_is_a_user CHECK (party_kind = 'user') NOT VALID;
--   COMMENT ON COLUMN deposits.party_kind IS 'Always `user` for a new row. Rows written before #245 may read `piggybank` (the seed deposits onto the retired fund claim); the CHECK is NOT VALID so they stay as history.';
--   INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, to_network, to_address, amount, reason, payload_hash,
--                         initiator_user_id, created_at, expires_at, decided_at, executed_withdrawal_id, failure_reason, version)
--     SELECT id, state, from_kind, from_id, to_kind, to_id, to_network, to_address, amount, reason, payload_hash,
--            initiator_user_id, created_at, expires_at, decided_at, executed_withdrawal_id, failure_reason, version
--       FROM retired.payments;
--   INSERT INTO payment_concierge_approval SELECT * FROM retired.payment_concierge_approval;
--   INSERT INTO payment_session_consent SELECT * FROM retired.payment_session_consent;
--   INSERT INTO scope_notification_outbox SELECT * FROM retired.scope_notification_outbox;
--   INSERT INTO event_log SELECT * FROM retired.event_log;
--   INSERT INTO outbox SELECT * FROM retired.outbox;
--   ALTER TABLE retired.<t> SET SCHEMA public;  -- for each of the 16 tables archived below
--   DROP TABLE retired.payments, retired.payment_concierge_approval, retired.payment_session_consent,
--              retired.scope_notification_outbox, retired.deposits, retired.event_log, retired.outbox;
--   DROP SCHEMA retired;
-- then, BEFORE the older image starts: DELETE FROM _sqlx_migrations WHERE version = 71;
-- — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version.
SET LOCAL lock_timeout = '3s';
SET LOCAL statement_timeout = '30s';

DO $preflight$
DECLARE
	n bigint;
BEGIN
	SELECT count(*) INTO n FROM book_orders WHERE state IN ('open', 'partially_filled');
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % open book order(s); cancel them on v2.20 before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM redemptions WHERE state = 'queued';
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % queued redemption(s); settle or fail them on v2.20 before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM unit_issuances WHERE state = 'queued';
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % queued unit issuance(s); let the v2.20 relay apply them before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM fee_policy_changes WHERE state IN ('awaiting_consilium', 'scheduled');
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % fee policy change(s) awaiting a vote or scheduled; cancel them on v2.20 before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM payments WHERE state IN ('pending', 'approved') AND (from_kind = 'service' OR to_kind = 'service');
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % open payment(s) naming an allocation; cancel them on v2.20 or let them expire before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM outbox
	 WHERE dispatched_at IS NULL AND compensated_at IS NULL
	   AND (kind IN ('allocations', 'book', 'fees', 'issuances', 'redemptions', 'subscriptions')
	     OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM payments WHERE from_kind = 'service' OR to_kind = 'service'))
	     OR (kind = 'balance' AND (payload->>'type' = 'deposited' AND payload->'party'->>'kind' IN ('user', 'treasury')) IS NOT TRUE)
	     OR (kind = 'withdrawals' AND payload->>'type' = 'settled'
	         AND payload->'payee' IS DISTINCT FROM '{"kind":"scope_account","id":{"scope":"main","label":"treasury"}}'::jsonb));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % undispatched or parked outbox row(s) of a removed unit path; let the v2.20 relay drain them (unpark or compensate a parked one) before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM payments p
	 WHERE p.from_kind = 'treasury' AND p.state = 'executed'
	   AND (p.to_kind = 'service' OR p.id IN (SELECT payment_id FROM payment_approval UNION SELECT payment_id FROM payment_consent));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % executed treasury payment(s) on a removed path; the reconciliation counts them from payments, so they cannot be archived — this deploy needs a decision first', n; END IF;

	SELECT count(*) INTO n FROM consilium_mail m JOIN payments p ON p.id = m.payment_id
	 WHERE (p.from_kind = 'service' OR p.to_kind = 'service')
	   AND NOT EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id)
	   AND NOT EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id);
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % governance mail(s) about a concierge- or session-seated payment; unexpected, inspect before deploying', n; END IF;

	-- 0072's checks, verbatim.
	SELECT count(*) INTO n FROM consilium WHERE state IN ('open', 'approved');
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % open or executing consilium; let it decide, expire or be cancelled on v2.20 before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM payments p WHERE p.state IN ('pending', 'approved')
	   AND (EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % payment(s) still waiting on a consilium or emailed-consent seat; let them end on v2.20 (within 7 days) before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM consilium_mail WHERE sent_at IS NULL AND withdrawn_at IS NULL AND attempts < 10;
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % governance mail(s) not yet sent; let the v2.20 mailer send them, or set withdrawn_at on them, before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM outbox WHERE dispatched_at IS NULL AND compensated_at IS NULL
	   AND (kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT payment_id FROM payment_approval UNION SELECT payment_id FROM payment_consent)));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % undispatched or parked outbox row(s) of the old approval path; let the v2.20 relay drain them before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM payments p
	 WHERE (EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id))
	   AND (EXISTS (SELECT 1 FROM payment_concierge_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_session_consent WHERE payment_id = p.id)
	     OR EXISTS (SELECT 1 FROM scope_notification_outbox WHERE payment_id = p.id));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % payment(s) seated on both an old and a new approval path; unexpected, inspect before deploying', n; END IF;
END
$preflight$;

CREATE SCHEMA retired;
COMMENT ON SCHEMA retired IS 'What only the removed unit and owner-governance paths understand (v2.21). Read by no code.';

ALTER TABLE allocation_access_grants SET SCHEMA retired;
ALTER TABLE allocations SET SCHEMA retired;
ALTER TABLE book_orders SET SCHEMA retired;
ALTER TABLE book_policies SET SCHEMA retired;
ALTER TABLE book_revisions SET SCHEMA retired;
ALTER TABLE book_trades SET SCHEMA retired;
ALTER TABLE fee_assessments SET SCHEMA retired;
ALTER TABLE fee_policies SET SCHEMA retired;
ALTER TABLE fee_policy_changes SET SCHEMA retired;
ALTER TABLE fee_settlements SET SCHEMA retired;
ALTER TABLE fund_positions SET SCHEMA retired;
ALTER TABLE fund_valuations SET SCHEMA retired;
ALTER TABLE redemptions SET SCHEMA retired;
ALTER TABLE subscriptions SET SCHEMA retired;
ALTER TABLE unit_issuances SET SCHEMA retired;
ALTER TABLE window_closures SET SCHEMA retired;

-- LIKE copies generated columns as plain stored ones, and no defaults.
CREATE TABLE retired.payments (LIKE public.payments, PRIMARY KEY (id));
CREATE TABLE retired.payment_concierge_approval (LIKE public.payment_concierge_approval, PRIMARY KEY (payment_id));
CREATE TABLE retired.payment_session_consent (LIKE public.payment_session_consent, PRIMARY KEY (payment_id));
CREATE TABLE retired.scope_notification_outbox (LIKE public.scope_notification_outbox, PRIMARY KEY (id));
CREATE TABLE retired.deposits (LIKE public.deposits, PRIMARY KEY (tx_ref));
CREATE TABLE retired.event_log (LIKE public.event_log, PRIMARY KEY (seq));
CREATE TABLE retired.outbox (LIKE public.outbox, PRIMARY KEY (seq));

CREATE TEMP TABLE retiring_payments ON COMMIT DROP AS
	SELECT id FROM payments p
	 WHERE (p.from_kind = 'service' OR p.to_kind = 'service')
	   AND NOT EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id)
	   AND NOT EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id);

WITH moved AS (DELETE FROM payment_concierge_approval WHERE payment_id IN (SELECT id FROM retiring_payments) RETURNING *)
INSERT INTO retired.payment_concierge_approval SELECT * FROM moved;
WITH moved AS (DELETE FROM payment_session_consent WHERE payment_id IN (SELECT id FROM retiring_payments) RETURNING *)
INSERT INTO retired.payment_session_consent SELECT * FROM moved;
WITH moved AS (DELETE FROM scope_notification_outbox WHERE payment_id IN (SELECT id FROM retiring_payments) RETURNING *)
INSERT INTO retired.scope_notification_outbox SELECT * FROM moved;
WITH moved AS (DELETE FROM payments WHERE id IN (SELECT id FROM retiring_payments) RETURNING *)
INSERT INTO retired.payments SELECT * FROM moved;
WITH moved AS (DELETE FROM deposits WHERE party_kind <> 'user' RETURNING *)
INSERT INTO retired.deposits SELECT * FROM moved;

WITH moved AS (
	DELETE FROM event_log
	 WHERE kind IN ('allocations', 'book', 'fees', 'issuances', 'redemptions', 'subscriptions')
	    OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM retiring_payments))
	    OR (kind = 'balance' AND (payload->>'type' = 'deposited' AND payload->'party'->>'kind' IN ('user', 'treasury')) IS NOT TRUE)
	    OR (kind = 'withdrawals' AND payload->>'type' = 'settled'
	        AND payload->'payee' IS DISTINCT FROM '{"kind":"scope_account","id":{"scope":"main","label":"treasury"}}'::jsonb)
	RETURNING *
)
INSERT INTO retired.event_log SELECT * FROM moved;
WITH moved AS (
	DELETE FROM outbox
	 WHERE kind IN ('allocations', 'book', 'fees', 'issuances', 'redemptions', 'subscriptions')
	    OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM retiring_payments))
	    OR (kind = 'balance' AND (payload->>'type' = 'deposited' AND payload->'party'->>'kind' IN ('user', 'treasury')) IS NOT TRUE)
	    OR (kind = 'withdrawals' AND payload->>'type' = 'settled'
	        AND payload->'payee' IS DISTINCT FROM '{"kind":"scope_account","id":{"scope":"main","label":"treasury"}}'::jsonb)
	RETURNING *
)
INSERT INTO retired.outbox SELECT * FROM moved;

ALTER TABLE deposits VALIDATE CONSTRAINT deposits_party_is_a_user;
COMMENT ON COLUMN deposits.party_kind IS NULL;
