-- A pending top-up that reaches `stop_after` is read back from the provider one last time before it is given up
-- (ANR-1473, ADR 0003 amended 2026-11-28). `expired` keeps one meaning: a last read still showed it pending.
--
-- `ob_claim_expiring_top_ups` claims the pending top-ups at least `p_stop_after_micros` old that were not claimed in
-- the last `p_read_again_micros` (the same touch of `last_read_at` as the due claim, so two callers never get the
-- same one and a crash between claim and settle leaves the row pending, claimed again after `p_read_again_micros`),
-- the oldest first. It is disjoint from `ob_claim_pending_top_ups`, which takes the rows younger than `stop_after`.
-- Plain SQL: a SQL-standard body; `ob_top_up_pending` serves the ORDER BY.
CREATE FUNCTION ob_claim_expiring_top_ups(p_stop_after_micros bigint, p_read_again_micros bigint, p_limit integer)
    RETURNS TABLE (o_payment_id text)
    LANGUAGE sql
BEGIN ATOMIC
    WITH picked AS MATERIALIZED (
        SELECT d.payment_id FROM ob_top_up d
        WHERE d.state = 'pending'
          AND d.created_at <= now() - (make_interval(secs => (p_stop_after_micros / 1000000)::double precision)
                                       + (p_stop_after_micros % 1000000) * interval '1 microsecond')
          AND (d.last_read_at IS NULL
               OR d.last_read_at <= now() - (make_interval(secs => (p_read_again_micros / 1000000)::double precision)
                                             + (p_read_again_micros % 1000000) * interval '1 microsecond'))
        ORDER BY d.created_at
        LIMIT p_limit
        FOR UPDATE SKIP LOCKED)
    UPDATE ob_top_up t SET last_read_at = clock_timestamp()
    FROM picked p
    WHERE t.payment_id = p.payment_id
    RETURNING t.payment_id;
END;

-- Moves one top-up to `expired`, only when it is still pending and at least `p_stop_after_micros` old, and answers
-- whether it did. A top-up settled meanwhile (a webhook, another pass) is no longer pending: the update matches no
-- row and answers false, and the guard is never reached. Plain SQL: a SQL-standard body.
CREATE FUNCTION ob_expire_top_up(p_payment_id text, p_stop_after_micros bigint)
    RETURNS boolean
    LANGUAGE sql
BEGIN ATOMIC
    WITH moved AS (
        UPDATE ob_top_up SET state = 'expired'
        WHERE payment_id = p_payment_id
          AND state = 'pending'
          AND created_at <= now() - (make_interval(secs => (p_stop_after_micros / 1000000)::double precision)
                                     + (p_stop_after_micros % 1000000) * interval '1 microsecond')
        RETURNING 1)
    SELECT count(*) = 1 FROM moved;
END;
