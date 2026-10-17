-- 0066: payments decided over concierge's approvals and by the investor's own session, and
-- the outbox of notices banking sends through concierge's scope inbox. Expand only: three
-- new tables, nothing existing is touched, and the consilium / emailed-consent path keeps
-- working beside them (org restructure plan §4.4, the old path leaves in the contract step).
--
-- WHY A SEAT TABLE PER WAY OF BEING DECIDED. A payment order needs exactly one approval, and
-- which one is a function of its source (`domain::payments`): fund-owned money is decided by
-- the platform owners, an investor's claim by that investor. Until now the first was a
-- banking-run consilium (`payment_approval`) and the second an emailed token
-- (`payment_consent`). The first becomes a `bank:payment` approval in concierge, the second
-- the investor's own answer inside the cabinet. The old seats stay for the orders already
-- open and for the switches (`PAYMENT_APPROVALS_FUND` / `PAYMENT_APPROVALS_SESSION`, `consilium`); a payment has at most
-- one seat of the four, which the application checks and the composite keys below make
-- unrepresentable for the wrong source.
--
-- REVERSIBILITY. The way back is the FLAG, not the image: set `PAYMENT_APPROVALS_FUND=consilium` and `PAYMENT_APPROVALS_SESSION=consilium`
-- (new orders then use the old path), wait until every order seated on these tables has ended
-- (`SELECT count(*) FROM payments p WHERE p.state IN ('pending', 'approved') AND (EXISTS (SELECT
-- 1 FROM payment_concierge_approval WHERE payment_id = p.id) OR EXISTS (SELECT 1 FROM
-- payment_session_consent WHERE payment_id = p.id))` is 0 — they end by themselves, within 7 days),
-- and only then, if at all, roll anything back. An image older than this release cannot even
-- propose: concierge already holds a newer catalog than the old image's.
--   The tables may stay for good: the rows are history, nothing reads them once the flag is
--   off, and nothing is lost by leaving them. If they are dropped,
--   DROP TABLE IF EXISTS scope_notification_outbox, payment_session_consent, payment_concierge_approval;
--   then, as `postgres`, BEFORE an older image starts:
--   DELETE FROM _sqlx_migrations WHERE version = 66;
--   — `sqlx::migrate!()` refuses to boot a binary that does not know an applied version.
SET LOCAL lock_timeout = '3s';
SET LOCAL statement_timeout = '30s';

-- The owners' seat: the concierge approval a fund-owned order waits on.
--
-- `payload_hash` and `destination` are written with the order and never change: the approval
-- is proposed from them, and the decision pulled back is checked against them. The
-- destination is stored rendered (the label plus the recognisable detail it had at open)
-- because the detail — a mailbox, a product's title — can change while the order waits, and
-- a hash recomputed from today's detail would no longer be the one the owners voted on.
--
-- `approval_id` is NULL until the proposal is opened. The request that wrote the order opens
-- it, with the proof of who asked (concierge keeps that person off the roster); an order that
-- stays NULL is a request that died between the two and is closed by the executor, never
-- proposed later without the proof. `reported_at` is set once the outcome
-- is told to concierge; `withdrawn_at` once the proposal of an order that died first was
-- taken back.
CREATE TABLE IF NOT EXISTS payment_concierge_approval (
    payment_id      UUID        PRIMARY KEY REFERENCES payments (id) ON DELETE CASCADE,
    -- Always TRUE, and only ever TRUE: the second key of the composite FK, which is what
    -- keeps this seat off an investor's order.
    fund_owned      BOOLEAN     NOT NULL DEFAULT TRUE CHECK (fund_owned),
    destination     TEXT        NOT NULL CHECK (octet_length(destination) BETWEEN 1 AND 200),
    -- SHA-256 hex of the approval payload banking sent (`ev-approval-v1`, concierge's
    -- `payload_hash`): what the opened and the pulled approval must both carry.
    payload_hash    TEXT        NOT NULL CHECK (payload_hash ~ '^[0-9a-f]{64}$'),
    approval_id     TEXT        UNIQUE CHECK (approval_id IS NULL OR octet_length(approval_id) BETWEEN 1 AND 128),
    approval_level  TEXT        CHECK (approval_level IS NULL OR approval_level IN ('platform', 'scope')),
    attempts        INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error      TEXT,
    withdrawn_at    TIMESTAMPTZ,
    reported_at     TIMESTAMPTZ,
    -- Unix seconds after which concierge voids the passed decision unexecuted (its
    -- `execute_by`), recorded when the decision is carried here; past it the order is not
    -- executed. NULL until then.
    execute_by      BIGINT      CHECK (execute_by IS NULL OR execute_by > 0),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (payment_id, fund_owned) REFERENCES payments (id, fund_owned) ON DELETE CASCADE
);

-- The executor's probes: orders whose proposal is not opened yet, and those whose approval is
-- still to be watched. Both are tiny sets; the partial indexes keep them so.
CREATE INDEX IF NOT EXISTS payment_concierge_approval_unopened_idx ON payment_concierge_approval (next_attempt_at) WHERE approval_id IS NULL;
CREATE INDEX IF NOT EXISTS payment_concierge_approval_unreported_idx ON payment_concierge_approval (created_at) WHERE approval_id IS NOT NULL AND reported_at IS NULL;

-- The investor's seat: their own answer, given signed in to the cabinet. No token and no code:
-- the session is the credential, and the cabinet gate asks for a fresh login before an
-- approval (a decline needs none).
--
-- The two pins are `payment_consent`'s, with the same meaning: the subject's folded revoke
-- floor and the digest of their mailbox at open, re-checked when they answer and again when
-- the money is about to move, fail-closed — a `RevokeTokens` or a changed mailbox in between
-- voids the consent. The composite FK is `payment_consent`'s too: the seat can only name its
-- payment's actual source user, which the database generates from `from_kind`/`from_id`.
CREATE TABLE IF NOT EXISTS payment_session_consent (
    payment_id                    UUID        PRIMARY KEY REFERENCES payments (id) ON DELETE CASCADE,
    subject_user_id               UUID        NOT NULL REFERENCES users (id),
    decision                      TEXT        NOT NULL DEFAULT 'pending' CHECK (decision IN ('pending', 'approve', 'reject')),
    decided_at                    TIMESTAMPTZ,
    subject_token_version_at_open BIGINT      NOT NULL CHECK (subject_token_version_at_open >= 0),
    subject_email_hash_at_open    BYTEA       NOT NULL CHECK (octet_length(subject_email_hash_at_open) = 32),
    -- What the subject personally answered over: the order's hash at the moment of the answer.
    payload_hash                  BYTEA       CHECK (payload_hash IS NULL OR octet_length(payload_hash) = 32),
    -- Audit only: the edge supplies these and they are never trusted for authorization.
    client_ip                     TEXT,
    user_agent                    TEXT,
    created_at                    TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (payment_id, subject_user_id) REFERENCES payments (id, source_user_id) ON DELETE CASCADE,
    CONSTRAINT payment_session_consent_decision_is_atomic CHECK ((decision = 'pending') = (decided_at IS NULL))
);

-- "What is waiting for me": the cabinet's list, per subject, over the unanswered seats only.
CREATE INDEX IF NOT EXISTS payment_session_consent_pending_idx ON payment_session_consent (subject_user_id) WHERE decision = 'pending';

-- Notices to a person about their money, through concierge's scope inbox
-- (`ScopeInboxService.EmitScopeNotification`, topic `bank:money-movement`). The queue the
-- governance mail used for the same purpose (`consilium_mail`) is not reused: it drains into
-- `SendGovernanceMail`, which the new path no longer depends on.
--
-- Written in the same transaction as the fact it announces, so a notice can never be lost to
-- a concierge outage nor sent for a fact that rolled back; delivered afterwards by a
-- singleton worker. The emit is idempotent by `dedupe_key`, so redelivery is free.
-- `user_id` is the recipient's BANKING id: the worker resolves the concierge id at send time,
-- in the plane that owns identities.
CREATE TABLE IF NOT EXISTS scope_notification_outbox (
    id              BIGSERIAL   PRIMARY KEY,
    user_id         UUID        NOT NULL REFERENCES users (id),
    topic           TEXT        NOT NULL CHECK (topic ~ '^bank:[a-z0-9_-]+(:[a-z0-9_-]+)*$' AND octet_length(topic) <= 64),
    kind            TEXT        NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 64),
    title           TEXT        NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    body            TEXT        NOT NULL CHECK (char_length(body) <= 2000),
    link            TEXT        NOT NULL CHECK (char_length(link) <= 512),
    dedupe_key      TEXT        NOT NULL UNIQUE CHECK (char_length(dedupe_key) BETWEEN 1 AND 128),
    -- The payment a notice is about, so an invitation to a payment that died can be taken back.
    payment_id      UUID        REFERENCES payments (id) ON DELETE CASCADE,
    occurred_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at         TIMESTAMPTZ,
    withdrawn_at    TIMESTAMPTZ,
    -- A failure of the message itself (charged) versus a wait for concierge or its catalog
    -- (not charged): the same split `consilium_mail` draws.
    attempts        INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    deferrals       INTEGER     NOT NULL DEFAULT 0 CHECK (deferrals >= 0),
    next_attempt_at TIMESTAMPTZ,
    last_error      TEXT,
    CONSTRAINT scope_notification_outbox_withdrawn_is_unsent CHECK (withdrawn_at IS NULL OR sent_at IS NULL)
);

CREATE INDEX IF NOT EXISTS scope_notification_outbox_due_idx ON scope_notification_outbox (id) WHERE sent_at IS NULL AND withdrawn_at IS NULL AND attempts < 10;
CREATE INDEX IF NOT EXISTS scope_notification_outbox_payment_idx ON scope_notification_outbox (payment_id) WHERE payment_id IS NOT NULL;
