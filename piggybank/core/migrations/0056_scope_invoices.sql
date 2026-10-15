-- 0056: invoices a scope issues and a user pays (`scope.v1.InvoiceService`,
-- `banking.v1.InvoicePaymentService`). Expand only: one new table and one new column with a
-- constant default on `money_events`. The money is in TigerBeetle — a payment is one posted
-- transfer `<payer's main or compartment> → scope_account:<scope>:<label>` (transfer code 58);
-- these rows are who asked whom for what, and who paid it from where.
--
-- Every statement is IF NOT EXISTS, so re-applying after the rollback below, with the table and
-- the column kept, does not fail on them.
--
-- REVERSIBILITY: DROP TABLE IF EXISTS scope_invoices;
--   ALTER TABLE money_events DROP COLUMN IF EXISTS paid_from;
--   Safe only while no invoice has been paid. Once one has, its TigerBeetle transfer outlives
--   the row: drop it and the ledger still shows the user's money in the scope's account with
--   nothing in Postgres saying which invoice it settled.
--   Rolling the image back past this migration also needs, as `postgres`, BEFORE the older
--   image starts: DELETE FROM _sqlx_migrations WHERE version = 56;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version. The
--   table and the column may stay instead of being dropped: the older binary never reads the
--   table, and its inserts into `money_events` get the column's default. An older relay meeting
--   a `scope_invoices` outbox event dispatches it without posting it (it does not know the
--   kind), so roll back only with no payment waiting in the outbox.
SET LOCAL lock_timeout = '3s';

-- One invoice. `amount` is 18-dp base units as TEXT, like every amount the control plane
-- keeps. `version` is the invoice's last event's. The payment columns are set together, once,
-- when the invoice is paid; `pay_idempotency_key` is the payer's key for that payment, so a
-- repeat of it finds this row.
CREATE TABLE IF NOT EXISTS scope_invoices (
	id                   UUID PRIMARY KEY,
	client_id            TEXT NOT NULL CHECK (client_id ~ '^[a-z][a-z0-9_-]{0,63}$'),
	scope                TEXT NOT NULL CHECK (scope ~ '^[a-z][a-z0-9_]{0,31}$' AND scope <> 'main'),
	-- banking's own labels never take a client's invoice (see the domain's `Invoice::restore`).
	to_label             TEXT NOT NULL CHECK (to_label ~ '^[a-z0-9_:.-]{1,64}$' AND to_label NOT IN ('fee', 'treasury')),
	amount               TEXT NOT NULL CHECK (amount ~ '^[1-9][0-9]*$'),
	memo                 TEXT NOT NULL DEFAULT '' CHECK (length(memo) <= 140),
	-- NULL: anyone signed in may pay.
	payer_auth_subject   TEXT CHECK (length(payer_auth_subject) BETWEEN 1 AND 255),
	subject_ref          TEXT NOT NULL CHECK (length(subject_ref) BETWEEN 1 AND 128),
	state                TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'paid', 'cancelled', 'expired')),
	version              BIGINT NOT NULL DEFAULT 1 CHECK (version >= 1),
	expires_at           TIMESTAMPTZ NOT NULL,
	paid_by_user         UUID REFERENCES users (id),
	paid_by_auth_subject TEXT,
	-- `main`, or the invoice's own scope (the payer's compartment there).
	paid_from            TEXT CHECK (paid_from = 'main' OR paid_from = scope),
	paid_at              TIMESTAMPTZ,
	pay_idempotency_key  TEXT CHECK (length(pay_idempotency_key) BETWEEN 1 AND 128),
	-- The payment's ledger leg: `pending` from the moment it is paid until the relay posts it,
	-- then `posted`; a refused or parked leg puts the invoice back to `open` (NULL again) and
	-- leaves its reason in `pay_failure_reason`. Only `posted` is a payment anyone is told of.
	ledger_state         TEXT CHECK (ledger_state IN ('pending', 'posted')),
	pay_failure_reason   TEXT,
	created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
	updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
	CHECK ((state = 'paid') = (paid_by_user IS NOT NULL)),
	CHECK ((paid_by_user IS NULL) = (paid_by_auth_subject IS NULL)),
	CHECK ((paid_by_user IS NULL) = (paid_from IS NULL)),
	CHECK ((paid_by_user IS NULL) = (paid_at IS NULL)),
	CHECK ((paid_by_user IS NULL) = (pay_idempotency_key IS NULL)),
	CHECK ((paid_by_user IS NULL) = (ledger_state IS NULL))
);
-- A payer's key names one payment: the repeat finds it, and a racer past the key lock cannot
-- pay a second invoice under it.
CREATE UNIQUE INDEX IF NOT EXISTS scope_invoices_pay_key_idx ON scope_invoices (paid_by_user, pay_idempotency_key) WHERE paid_by_user IS NOT NULL;
-- The reaper's scan for open invoices past their expiry.
CREATE INDEX IF NOT EXISTS scope_invoices_due_idx ON scope_invoices (expires_at) WHERE state = 'open';
-- A payer's payments not yet on the ledger — promised away from their source in every spend
-- check — and the reaper's scan for one whose event was lost.
CREATE INDEX IF NOT EXISTS scope_invoices_pending_idx ON scope_invoices (paid_by_user) WHERE ledger_state = 'pending';

-- Where an invoice was paid from (`MoneyEvent.paid_from`): `main` or the scope. A constant
-- default keeps the column metadata-only: no table rewrite, no long lock.
ALTER TABLE money_events ADD COLUMN IF NOT EXISTS paid_from TEXT NOT NULL DEFAULT '';
