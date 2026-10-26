-- 0072: approvals are concierge's — the CONTRACT half of the old governance path's removal
-- (v2.21, #569).
--
-- v2.21's code reads no consilium, voter, governance mail, emailed consent or roster
-- change, and a payment with no live seat is a repository error. So, unchanged, into
-- schema `retired`:
--   * whole tables: consilium, consilium_voter, consilium_mail, payment_approval,
--     payment_consent, governance_roster_change — their keys into `payments` are dropped,
--     since the payments they name move too (a CASCADE would empty the archive);
--   * payments seated on payment_approval or payment_consent, and their `payments` events;
--   * `consilium` events.
-- Then, over what is left live:
--   * `payments` CHECKs narrowed to the parties v2.21 has — a source is a person or the
--     treasury, a destination a person (validated: 0071 and this step took every other row);
--   * the treasury's requirement renamed `owner_consilium` → `owners_approval` in the event
--     log (the only place it is stored; the approval payload and hash never held it);
--   * `fund_owned` → `treasury_source` on payments and payment_concierge_approval, constraint
--     names with it. The expression stays `from_kind <> 'user'`, equal to
--     `from_kind = 'treasury'` under the narrowed CHECK.
-- A postcondition refuses if any live event_log / outbox row is a kind or type v2.21 cannot
-- read: such history must be named in a retire predicate, never swept unread.
--
-- PREFLIGHT. Refuses while the old path still has live state (0071 runs the same checks).
--
-- REVERSIBILITY: before 0071's, as `postgres`, statement for statement
-- (`tests/contract_migrations.rs` runs it):
--   ALTER TABLE payment_concierge_approval RENAME CONSTRAINT payment_concierge_approval_payment_id_treasury_source_fkey TO payment_concierge_approval_payment_id_fund_owned_fkey;
--   ALTER TABLE payment_concierge_approval RENAME CONSTRAINT payment_concierge_approval_treasury_source_check TO payment_concierge_approval_fund_owned_check;
--   ALTER TABLE payment_concierge_approval RENAME COLUMN treasury_source TO fund_owned;
--   ALTER TABLE payments RENAME CONSTRAINT payments_id_treasury_source_key TO payments_id_fund_owned_key;
--   ALTER TABLE payments RENAME COLUMN treasury_source TO fund_owned;
--   UPDATE event_log SET payload = jsonb_set(payload, '{requirement,requirement}', '"owner_consilium"')
--    WHERE kind = 'payments' AND payload->'requirement'->>'requirement' = 'owners_approval';
--   ALTER TABLE payments DROP CONSTRAINT payments_from_kind_check, DROP CONSTRAINT payments_to_kind_check,
--     ADD CONSTRAINT payments_from_kind_check CHECK (from_kind IN ('user', 'service', 'treasury')),
--     ADD CONSTRAINT payments_to_kind_check CHECK (to_kind IN ('user', 'service'));
--   CREATE TEMP TABLE restoring_payments ON COMMIT DROP AS
--     SELECT payment_id AS id FROM retired.payment_approval UNION SELECT payment_id FROM retired.payment_consent;
--   INSERT INTO payments (id, state, from_kind, from_id, to_kind, to_id, to_network, to_address, amount, reason, payload_hash,
--                         initiator_user_id, created_at, expires_at, decided_at, executed_withdrawal_id, failure_reason, version)
--     SELECT id, state, from_kind, from_id, to_kind, to_id, to_network, to_address, amount, reason, payload_hash,
--            initiator_user_id, created_at, expires_at, decided_at, executed_withdrawal_id, failure_reason, version
--       FROM retired.payments WHERE id IN (SELECT id FROM restoring_payments);
--   INSERT INTO event_log SELECT * FROM retired.event_log WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM restoring_payments));
--   INSERT INTO outbox SELECT * FROM retired.outbox WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM restoring_payments));
--   DELETE FROM retired.event_log WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM restoring_payments));
--   DELETE FROM retired.outbox WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM restoring_payments));
--   DELETE FROM retired.payments WHERE id IN (SELECT id FROM restoring_payments);
--   ALTER TABLE retired.<t> SET SCHEMA public;  -- for each of the 6 tables archived below
--   ALTER TABLE consilium ADD CONSTRAINT consilium_executed_payment_fk FOREIGN KEY (executed_payment_id) REFERENCES payments (id);
--   ALTER TABLE consilium_mail ADD CONSTRAINT consilium_mail_payment_id_fkey FOREIGN KEY (payment_id) REFERENCES payments (id) ON DELETE CASCADE;
--   ALTER TABLE payment_approval ADD CONSTRAINT payment_approval_payment_id_fkey FOREIGN KEY (payment_id) REFERENCES payments (id) ON DELETE CASCADE,
--     ADD CONSTRAINT payment_approval_payment_id_fund_owned_fkey FOREIGN KEY (payment_id, fund_owned) REFERENCES payments (id, fund_owned) ON DELETE CASCADE;
--   ALTER TABLE payment_consent ADD CONSTRAINT payment_consent_payment_id_fkey FOREIGN KEY (payment_id) REFERENCES payments (id) ON DELETE CASCADE,
--     ADD CONSTRAINT payment_consent_payment_id_subject_user_id_fkey FOREIGN KEY (payment_id, subject_user_id) REFERENCES payments (id, source_user_id) ON DELETE CASCADE;
-- then, BEFORE the older image starts: DELETE FROM _sqlx_migrations WHERE version = 72;
-- — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version.
SET LOCAL lock_timeout = '3s';
SET LOCAL statement_timeout = '30s';

DO $preflight$
DECLARE
	n bigint;
BEGIN
	SELECT count(*) INTO n FROM consilium WHERE state IN ('open', 'approved');
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % open or executing consilium; let it decide, expire or be cancelled on v2.20 before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM payments p WHERE p.state IN ('pending', 'approved')
	   AND (EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % payment(s) still waiting on a consilium or emailed-consent seat; let them end on v2.20 (within 7 days) before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM consilium_mail WHERE sent_at IS NULL AND withdrawn_at IS NULL AND attempts < 10; -- the v2.20 mailer's MAX_ATTEMPTS
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % governance mail(s) not yet sent; let the v2.20 mailer send them, or set withdrawn_at on them, before deploying v2.21', n; END IF;

	SELECT count(*) INTO n FROM outbox WHERE dispatched_at IS NULL AND compensated_at IS NULL
	   AND (kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT payment_id FROM payment_approval UNION SELECT payment_id FROM payment_consent)));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % undispatched or parked outbox row(s) of the old approval path; let the v2.20 relay drain them before deploying v2.21', n; END IF;

	-- deleting such a payment would CASCADE through a live seat or notice
	SELECT count(*) INTO n FROM payments p
	 WHERE (EXISTS (SELECT 1 FROM payment_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_consent WHERE payment_id = p.id))
	   AND (EXISTS (SELECT 1 FROM payment_concierge_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM payment_session_consent WHERE payment_id = p.id)
	     OR EXISTS (SELECT 1 FROM scope_notification_outbox WHERE payment_id = p.id));
	IF n > 0 THEN RAISE EXCEPTION 'contract preflight: % payment(s) seated on both an old and a new approval path; unexpected, inspect before deploying', n; END IF;
END
$preflight$;

ALTER TABLE consilium SET SCHEMA retired;
ALTER TABLE consilium_voter SET SCHEMA retired;
ALTER TABLE consilium_mail SET SCHEMA retired;
ALTER TABLE payment_approval SET SCHEMA retired;
ALTER TABLE payment_consent SET SCHEMA retired;
ALTER TABLE governance_roster_change SET SCHEMA retired;

ALTER TABLE retired.consilium DROP CONSTRAINT consilium_executed_payment_fk;
ALTER TABLE retired.consilium_mail DROP CONSTRAINT consilium_mail_payment_id_fkey;
ALTER TABLE retired.payment_approval DROP CONSTRAINT payment_approval_payment_id_fkey, DROP CONSTRAINT payment_approval_payment_id_fund_owned_fkey;
ALTER TABLE retired.payment_consent DROP CONSTRAINT payment_consent_payment_id_fkey, DROP CONSTRAINT payment_consent_payment_id_subject_user_id_fkey;

CREATE TEMP TABLE retiring_payments ON COMMIT DROP AS
	SELECT payment_id AS id FROM retired.payment_approval UNION SELECT payment_id FROM retired.payment_consent;

WITH moved AS (DELETE FROM event_log WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM retiring_payments)) RETURNING *)
INSERT INTO retired.event_log SELECT * FROM moved;
WITH moved AS (DELETE FROM outbox WHERE kind = 'consilium' OR (kind = 'payments' AND aggregate_id IN (SELECT id FROM retiring_payments)) RETURNING *)
INSERT INTO retired.outbox SELECT * FROM moved;
WITH moved AS (DELETE FROM payments WHERE id IN (SELECT id FROM retiring_payments) RETURNING *)
INSERT INTO retired.payments SELECT * FROM moved;

ALTER TABLE payments
	DROP CONSTRAINT payments_from_kind_check,
	DROP CONSTRAINT payments_to_kind_check,
	ADD CONSTRAINT payments_from_kind_check CHECK (from_kind IN ('user', 'treasury')),
	ADD CONSTRAINT payments_to_kind_check CHECK (to_kind = 'user');

UPDATE event_log SET payload = jsonb_set(payload, '{requirement,requirement}', '"owners_approval"')
 WHERE kind = 'payments' AND payload->'requirement'->>'requirement' = 'owner_consilium';

ALTER TABLE payments RENAME COLUMN fund_owned TO treasury_source;
ALTER TABLE payments RENAME CONSTRAINT payments_id_fund_owned_key TO payments_id_treasury_source_key;
ALTER TABLE payment_concierge_approval RENAME COLUMN fund_owned TO treasury_source;
ALTER TABLE payment_concierge_approval RENAME CONSTRAINT payment_concierge_approval_fund_owned_check TO payment_concierge_approval_treasury_source_check;
ALTER TABLE payment_concierge_approval RENAME CONSTRAINT payment_concierge_approval_payment_id_fund_owned_fkey TO payment_concierge_approval_payment_id_treasury_source_fkey;

DO $postcondition$
DECLARE
	stray text;
BEGIN
	SELECT string_agg(DISTINCT kind || '/' || coalesce(payload->>'type', '?'), ', ') INTO stray
	  FROM (SELECT kind, payload FROM event_log UNION ALL SELECT kind, payload FROM outbox) e
	 WHERE ((kind, payload->>'type') IN (VALUES
	           ('users', 'provisioned'), ('users', 'email_changed'), ('users', 'profile_updated'), ('users', 'tokens_revoked'), ('users', 'disabled'),
	           ('withdrawals', 'requested'), ('withdrawals', 'dispatched'), ('withdrawals', 'settled'), ('withdrawals', 'failed'), ('withdrawals', 'cancelled'),
	           ('payments', 'opened'), ('payments', 'approval_recorded'), ('payments', 'approved'), ('payments', 'reserved'), ('payments', 'settled'),
	           ('payments', 'executed'), ('payments', 'released'), ('payments', 'execution_failed'), ('payments', 'rejected'), ('payments', 'expired'),
	           ('payments', 'cancelled'),
	           ('balance', 'deposited'),
	           ('compartments', 'moved'),
	           ('scope_ledger', 'transferred'),
	           ('scope_holds', 'placed'), ('scope_holds', 'captured'), ('scope_holds', 'released'),
	           ('scope_payouts', 'paid_out'),
	           ('scope_invoices', 'created'), ('scope_invoices', 'paid'), ('scope_invoices', 'cancelled'), ('scope_invoices', 'expired'), ('scope_invoices', 'payment_failed'),
	           ('scope_charges', 'charged'))) IS NOT TRUE
	    OR (kind IN ('payments', 'balance', 'withdrawals') AND jsonb_path_exists(payload, '$.** ? (@.kind == "service")'))
	    OR (kind = 'payments' AND (payload->>'tier' = 'service' OR payload->'requirement'->>'requirement' = 'owner_consilium'));
	IF stray IS NOT NULL THEN RAISE EXCEPTION 'contract postcondition: live events v2.21 cannot read: %; name them in a retire predicate', stray; END IF;
END
$postcondition$;
