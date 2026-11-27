-- Always book as debt: the owner's decision of 2026-11-27 (ADR 0001, amended, E1 to E4).
--
-- "Always book as debt": usage that already happened is always booked, even past the owner's debt limit. The limit
-- refuses only new calls, through `ob_allowed`, and an owner past it is flagged by the reads. The drain keeps one
-- refusal of its own besides `unknown_account` and `id_reused`: a debit whose debt would pass the largest amount is
-- refused `overflow` (a different rule, not the limit), never wrapped or clamped.

-- ---------------------------------------------------------------------------------------------------------------
-- The reasons a drain can give. `limit` is gone. A database whose outcomes hold `limit` fails here rather than keep
-- a reason nothing reads; none does: no release ran the drain with a limit before this migration.

ALTER TABLE ob_event_outcome DROP CONSTRAINT ob_event_outcome_reason;
ALTER TABLE ob_event_outcome ADD CONSTRAINT ob_event_outcome_reason CHECK (
    (state = 'refused') = (reason IS NOT NULL)
    AND (reason IS NULL OR reason IN ('no_price', 'unknown_account', 'overflow')));

ALTER TABLE ob_usage_event DROP CONSTRAINT ob_usage_event_refusal;
ALTER TABLE ob_usage_event ADD CONSTRAINT ob_usage_event_refusal CHECK (
    (state = 'refused') = (refusal IS NOT NULL)
    AND (refusal IS NULL OR refusal IN ('no_price', 'unknown_account', 'id_reused', 'overflow')));

-- ---------------------------------------------------------------------------------------------------------------
-- The drain (ADR 0001, points 11 to 15, 22 and 23, amended), again. `plpgsql`: it loops over the batch, and each
-- event's draw must see the lots and the debt the one before it left. Steps 1 to 3 and 5 are those of `…006`.
--
-- 1. Take up to `p_limit` received events, oldest received first, skipping rows another drain holds.
-- 2. Claim each event id once, in id order, by inserting its outcome as pending; a row whose id already has an
--    outcome (another occurrence day, or earlier in this batch) is refused `id_reused` and moves no money (point 15).
-- 3. Lock the balance rows of the claimed events, in (owner, currency) order.
-- 4. Decide each claimed event, owner by owner: no account in the currency is `unknown_account`; a debit that would
--    take the owner's debt past the largest amount is `overflow`; otherwise draw it across the owner's accounts and
--    their lots and book the rest as debt, against revenue, in one ledger transaction, **whatever the debt limit**.
-- 5. Move each event row to its outcome's state. Return one row per event taken, in the order taken.
CREATE OR REPLACE FUNCTION ob_drain(p_limit integer)
    RETURNS TABLE (o_event_id text, o_occurred_day integer, o_state text, o_reason text, o_amount bigint,
                   o_price_version uuid)
    LANGUAGE plpgsql AS $$
DECLARE
    v_ids      text[];
    v_days     date[];
    v_won_ids  text[];
    v_won_days date[];
    v_event    record;
    v_debt     bigint;
    v_avail    numeric;
    v_unpaid   numeric;
    v_txn      uuid;
    v_taken    bigint;
BEGIN
    SELECT coalesce(array_agg(t.event_id ORDER BY t.received_at, t.event_id), '{}'),
           coalesce(array_agg(t.occurred_on ORDER BY t.received_at, t.event_id), '{}')
    INTO v_ids, v_days
    FROM (SELECT e.event_id, e.occurred_on, e.received_at
          FROM ob_usage_event e
          WHERE e.state = 'received'
          ORDER BY e.received_at, e.event_id
          LIMIT greatest(p_limit, 0)
          FOR UPDATE SKIP LOCKED) t;
    IF cardinality(v_ids) = 0 THEN
        RETURN;
    END IF;

    WITH claimed AS (
        INSERT INTO ob_event_outcome (event_id, occurred_on, occurred_at, owner, currency, member, amount, state,
                                      meter, quantity, dimensions, price_version)
        SELECT e.event_id, e.occurred_on, e.occurred_at, e.owner, e.currency, e.member, e.amount, 'pending',
               e.meter, e.quantity, e.dimensions, e.price_version
        FROM ob_usage_event e
        JOIN unnest(v_ids, v_days) AS b(event_id, occurred_on)
          ON e.event_id = b.event_id AND e.occurred_on = b.occurred_on
        ORDER BY e.event_id, e.received_at, e.occurred_on
        ON CONFLICT (event_id) DO NOTHING
        RETURNING ob_event_outcome.event_id, ob_event_outcome.occurred_on
    )
    SELECT coalesce(array_agg(c.event_id), '{}'), coalesce(array_agg(c.occurred_on), '{}')
    INTO v_won_ids, v_won_days
    FROM claimed c;

    UPDATE ob_usage_event e SET state = 'refused', refusal = 'id_reused'
    FROM unnest(v_ids, v_days) AS b(event_id, occurred_on)
    WHERE e.event_id = b.event_id AND e.occurred_on = b.occurred_on
      AND NOT EXISTS (SELECT 1 FROM unnest(v_won_ids, v_won_days) AS w(event_id, occurred_on)
                      WHERE w.event_id = b.event_id AND w.occurred_on = b.occurred_on);

    PERFORM 1 FROM ob_balance bl
    WHERE (bl.owner, bl.currency) IN (SELECT o.owner, o.currency FROM ob_event_outcome o
                                      WHERE o.event_id = ANY (v_won_ids))
    ORDER BY bl.owner, bl.currency
    FOR UPDATE;

    FOR v_event IN
        SELECT o.event_id, o.owner, o.currency, o.amount
        FROM ob_event_outcome o
        WHERE o.event_id = ANY (v_won_ids)
        ORDER BY o.owner, o.currency, o.occurred_at, o.event_id
    LOOP
        SELECT bl.debt INTO v_debt
        FROM ob_balance bl WHERE bl.owner = v_event.owner AND bl.currency = v_event.currency;
        IF NOT FOUND THEN
            UPDATE ob_event_outcome o SET state = 'refused', reason = 'unknown_account'
            WHERE o.event_id = v_event.event_id;
            CONTINUE;
        END IF;
        IF v_event.amount = 0 THEN
            UPDATE ob_event_outcome o SET state = 'applied' WHERE o.event_id = v_event.event_id;
            CONTINUE;
        END IF;
        SELECT coalesce(sum(l.remaining), 0) INTO v_avail
        FROM ob_lot l JOIN ob_account a ON a.account_id = l.account_id
        WHERE a.owner = v_event.owner AND a.currency = v_event.currency AND l.remaining > 0
          AND (l.not_before IS NULL OR l.not_before <= now())
          AND (l.expires_at IS NULL OR l.expires_at > now());
        v_unpaid := greatest(v_event.amount - v_avail, 0);
        IF v_debt + v_unpaid > 9223372036854775807 THEN
            UPDATE ob_event_outcome o SET state = 'refused', reason = 'overflow' WHERE o.event_id = v_event.event_id;
            CONTINUE;
        END IF;
        v_txn := uuidv7();
        INSERT INTO ob_txn (txn_id, kind, currency, event_id)
            VALUES (v_txn, 'usage', v_event.currency, v_event.event_id);
        v_taken := ob_draw(v_txn, v_event.owner, v_event.currency, NULL, v_event.amount);
        PERFORM ob_add_debt(v_txn, v_event.owner, v_event.currency, v_event.amount - v_taken);
        INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
            VALUES (v_txn, v_event.currency, 'revenue', NULL, NULL, NULL, v_event.amount);
        UPDATE ob_event_outcome o SET state = 'applied' WHERE o.event_id = v_event.event_id;
    END LOOP;

    UPDATE ob_usage_event e SET state = o.state, refusal = o.reason
    FROM ob_event_outcome o
    JOIN unnest(v_won_ids, v_won_days) AS w(event_id, occurred_on)
      ON w.event_id = o.event_id AND w.occurred_on = o.occurred_on
    WHERE e.event_id = o.event_id AND e.occurred_on = o.occurred_on;

    RETURN QUERY
        SELECT e.event_id, ob_day(e.occurred_on), e.state, e.refusal, e.amount, e.price_version
        FROM unnest(v_ids, v_days) WITH ORDINALITY AS b(event_id, occurred_on, pos)
        JOIN ob_usage_event e ON e.event_id = b.event_id AND e.occurred_on = b.occurred_on
        ORDER BY b.pos;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- May `p_member` spend for the owner in the currency at `p_at_micros`? Refused, in this order:
--
-- - `debt_limit`: the owner owes something and its debt is at or past its limit. This is where the limit acts now:
--   it refuses new calls, never usage that already happened. A limit of zero refuses as soon as anything is owed;
--   with no limit, debt is unlimited and this never refuses;
-- - `balance`: what the drawable lots hold, less the debt, is zero or below (as before);
-- - `member_cap`: the member has a cap and their applied usage in the UTC month of `p_at_micros` reached it (as
--   before).
--
-- Same arguments and answer as `…004`'s, so it is replaced in place.
CREATE OR REPLACE FUNCTION ob_allowed(p_owner text, p_currency text, p_member text, p_at_micros bigint)
    RETURNS TABLE (o_refused text, o_balance bigint, o_member_remaining bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT CASE WHEN x.debt > 0 AND x.debt >= x.debt_limit THEN 'debt_limit'
                WHEN x.available - x.debt <= 0 THEN 'balance'
                WHEN x.remaining IS NOT NULL AND x.remaining <= 0 THEN 'member_cap'
           END,
           ob_clamp(x.available - x.debt),
           ob_clamp(x.remaining)
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
                WHERE m.owner = p_owner AND m.currency = p_currency AND m.member = p_member)
               - coalesce((SELECT sum(o.amount) FROM ob_event_outcome o
                           WHERE o.state = 'applied' AND o.owner = p_owner AND o.currency = p_currency
                             AND o.member = p_member
                             AND o.occurred_at >= date_trunc('month', ob_ts(p_at_micros), 'UTC')
                             AND o.occurred_at < date_trunc('month', ob_ts(p_at_micros), 'UTC') + interval '1 month'),
                          0) AS remaining
    ) x;
END;

-- ---------------------------------------------------------------------------------------------------------------
-- The flag: every owner and currency whose debt is past its limit. A plain view, no rule of its own beyond the
-- comparison; the API's balance read carries the same flag per owner (`pastLimit`).
CREATE VIEW ob_past_debt_limit AS
    SELECT b.owner, b.currency, b.debt, b.debt_limit
    FROM ob_balance b
    WHERE b.debt_limit IS NOT NULL AND b.debt > b.debt_limit;
