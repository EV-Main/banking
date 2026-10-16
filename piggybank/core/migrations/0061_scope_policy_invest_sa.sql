-- 0061: the move-in policy of the two seeded scopes, `invest` and `sa` (banking#523,
-- scope-ledger checklist item 13). 0052 seeded their `scope_policies` rows with the column
-- defaults — every user in, no limits — which is the state a payout or a debit from a
-- compartment must not run against. Data only, no schema change.
--
-- Owner values: KYC level 1, 1000 USDT per move, 5000 USDT moved in per user per rolling
-- 24 h, no cap on the balance. The automatic payout ceilings keep their defaults.
--
-- Each UPDATE touches a row only while it still holds exactly what 0052 wrote, so a policy
-- an operator has set since (docs/RUNBOOK-scope-policy.md) is never overwritten, and a
-- re-run changes nothing. The INSERT covers a database where the row is missing; it does
-- not touch an existing one. A scope that does not exist is skipped, never created. Scopes
-- registered from concierge's stream stay closed (`CLOSED_UNTIL_SET_KYC`, banking#511).
--
-- REVERSIBILITY: UPDATE scope_policies SET min_kyc_level = 0, max_per_move = NULL,
--   max_moved_in_24h = NULL, updated_at = now() WHERE scope IN ('invest', 'sa');
--   Safe at any time; it opens both scopes to every user again, so only do it on purpose.
SET LOCAL lock_timeout = '3s';

INSERT INTO scope_policies (scope, min_kyc_level, max_per_move, max_moved_in_24h)
SELECT id, 1, '1000000000000000000000', '5000000000000000000000'
  FROM scopes
 WHERE id IN ('invest', 'sa')
ON CONFLICT (scope) DO NOTHING;

UPDATE scope_policies
   SET min_kyc_level    = 1,
       max_per_move     = '1000000000000000000000',
       max_moved_in_24h = '5000000000000000000000',
       updated_at       = now()
 WHERE scope IN ('invest', 'sa')
   AND min_kyc_level = 0
   AND max_per_move IS NULL
   AND max_moved_in_24h IS NULL
   AND max_balance IS NULL;
