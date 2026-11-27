-- The deployer's policy for `ob_allowed`: the owner's decision of 2026-11-27 (ADR 0001, amended, F1 to F4; ADR 0002,
-- points 13 and 14, amended).
--
-- In the owner's words: "Its not the role of this service to judge that", and "I prefer this backend reports only
-- [...] plain comparison, no special case and let the end developer decide how in the .yaml configuration". So:
--
-- - each check of `ob_allowed` is a plain comparison of what is used against its bound, with no special case: the
--   `debt > 0` guard of `…007` is gone, and a debt limit of zero refuses an owner who owes nothing under
--   `at_or_above`, because 0 >= 0;
-- - which comparison, or none, is the deployer's, passed in as an argument (`ob_comparison`) per check. The rule
--   stays here; the service reads the choice from its configuration file and passes it on every call;
-- - the answer carries the numbers the verdict was made from, whatever the policy, so a caller with a rule of its
--   own reads them and ignores the verdict.

-- ---------------------------------------------------------------------------------------------------------------
-- A comparison: `at_or_above` refuses when used >= bound, `above` when used > bound, `never` never refuses. A domain,
-- so a call that names anything else, or nothing, fails at the call (23514, 23502) instead of answering.
CREATE DOMAIN ob_comparison AS text NOT NULL
    CONSTRAINT ob_comparison_mode CHECK (VALUE IN ('at_or_above', 'above', 'never'));

-- Whether `p_used` reached `p_bound` as `p_mode` compares them. No bound is never reached. Plain SQL.
CREATE FUNCTION ob_reached(p_mode ob_comparison, p_used numeric, p_bound numeric) RETURNS boolean
    LANGUAGE sql IMMUTABLE
    RETURN p_bound IS NOT NULL
        AND CASE p_mode
                WHEN 'at_or_above' THEN p_used >= p_bound
                WHEN 'above' THEN p_used > p_bound
                WHEN 'never' THEN false
            END;

-- ---------------------------------------------------------------------------------------------------------------
-- May `p_member` spend for the owner in the currency at `p_at_micros`? Three checks, in this order, each compared as
-- the deployer's mode for it says:
--
-- - `debt_limit`: the owner's debt against its debt limit (`p_debt_limit`). No limit never refuses;
-- - `balance`: the owner's debt against what its drawable lots hold (`p_balance`): `at_or_above` refuses a net
--   balance of zero or below, as `…004` did, `above` one below zero;
-- - `member_cap`: the member's applied usage in the UTC month of `p_at_micros` against their cap (`p_member_cap`).
--   No cap never refuses.
--
-- The numbers, whatever the modes: the net balance, what the lots hold, the debt, the limit, whether the debt is past
-- the limit (the view `ob_past_debt_limit`: debt > limit, the flag of the reads), the member's usage this month, the
-- cap and what is left of it. Amounts summed past the largest are clamped (`ob_clamp`); the comparisons are made on
-- the sums themselves.
--
-- Its arguments and its answer are not `…007`'s, so it is dropped and created again: nothing depends on it in the
-- schema (the API calls it from a query block, at run time).
DROP FUNCTION ob_allowed(text, text, text, bigint);

CREATE FUNCTION ob_allowed(p_owner text, p_currency text, p_member text, p_at_micros bigint,
                           p_debt_limit ob_comparison, p_balance ob_comparison, p_member_cap ob_comparison)
    RETURNS TABLE (o_refused text, o_balance bigint, o_available bigint, o_debt bigint, o_debt_limit bigint,
                   o_past_limit boolean, o_member_spent bigint, o_member_cap bigint, o_member_remaining bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT CASE WHEN ob_reached(p_debt_limit, x.debt, x.debt_limit) THEN 'debt_limit'
                WHEN ob_reached(p_balance, x.debt, x.available) THEN 'balance'
                WHEN ob_reached(p_member_cap, x.spent, x.cap) THEN 'member_cap'
           END,
           ob_clamp(x.available - x.debt),
           ob_clamp(x.available),
           x.debt,
           x.debt_limit,
           EXISTS (SELECT 1 FROM ob_past_debt_limit p WHERE p.owner = p_owner AND p.currency = p_currency),
           ob_clamp(x.spent),
           x.cap,
           ob_clamp(x.cap - x.spent)
    FROM (
        SELECT coalesce((SELECT sum(l.remaining) FROM ob_lot l JOIN ob_account a ON a.account_id = l.account_id
                         WHERE a.owner = p_owner AND a.currency = p_currency AND l.remaining > 0
                           AND (l.not_before IS NULL OR l.not_before <= ob_ts(p_at_micros))
                           AND (l.expires_at IS NULL OR l.expires_at > ob_ts(p_at_micros))), 0) AS available,
               coalesce((SELECT b.debt FROM ob_balance b
                         WHERE b.owner = p_owner AND b.currency = p_currency), 0) AS debt,
               (SELECT b.debt_limit FROM ob_balance b
                WHERE b.owner = p_owner AND b.currency = p_currency) AS debt_limit,
               (SELECT m.cap FROM ob_member_cap m
                WHERE m.owner = p_owner AND m.currency = p_currency AND m.member = p_member) AS cap,
               coalesce((SELECT sum(o.amount) FROM ob_event_outcome o
                         WHERE o.state = 'applied' AND o.owner = p_owner AND o.currency = p_currency
                           AND o.member = p_member
                           AND o.occurred_at >= date_trunc('month', ob_ts(p_at_micros), 'UTC')
                           AND o.occurred_at < date_trunc('month', ob_ts(p_at_micros), 'UTC') + interval '1 month'),
                        0) AS spent
    ) x;
END;
