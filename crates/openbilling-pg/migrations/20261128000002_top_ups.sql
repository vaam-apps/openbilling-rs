-- Top-ups (ADR 0003, amended 2026-11-28, ANR-1405): what openbilling asked the payment provider for is recorded before
-- the payer is sent away, so that
--   1. what the provider reports back is compared with what was asked for, and the credit is booked from the RECORDED
--      terms, never from the provider's metadata alone (a payment whose owner, label, currency or amount reads back
--      differently credits nothing, and stays refused); and
--   2. a paid top-up whose webhook never came is found again: the pending ones that are due are claimed and read back.
-- This reverses the alternative ADR 0003 rejected, "keep the target in a table" (amended there).
--
-- OB011 is raised when a payment is recorded again with other terms; the guard trigger below raises OB901 (a write to
-- an immutable row), the code the other guards use.

CREATE TABLE ob_top_up (
    payment_id     text        PRIMARY KEY,
    idem_key       text,
    owner          text        NOT NULL,
    currency       text        NOT NULL,
    label          text        NOT NULL,
    amount         bigint      NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    state          text        NOT NULL DEFAULT 'pending',
    mismatch_field text,
    last_read_at   timestamptz,
    CONSTRAINT ob_top_up_payment_id CHECK (ob_is_id(payment_id, 255)),
    CONSTRAINT ob_top_up_owner CHECK (ob_is_id(owner, 255)),
    CONSTRAINT ob_top_up_currency CHECK (ob_in_alphabet(currency, 16, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789')),
    CONSTRAINT ob_top_up_label CHECK (ob_in_alphabet(label, 64, 'abcdefghijklmnopqrstuvwxyz0123456789_-')),
    -- Only a payment that was never asked for (an `unrecorded` one, kept so an operator can see it) has no key.
    CONSTRAINT ob_top_up_key CHECK (idem_key IS NOT NULL OR coalesce(mismatch_field, '') = 'unrecorded'),
    CONSTRAINT ob_top_up_amount CHECK (amount > 0),
    CONSTRAINT ob_top_up_state CHECK (state IN ('pending', 'credited', 'canceled', 'mismatched', 'expired')),
    CONSTRAINT ob_top_up_mismatch CHECK ((state = 'mismatched') = (mismatch_field IS NOT NULL)),
    CONSTRAINT ob_top_up_mismatch_field
        CHECK (mismatch_field IS NULL OR mismatch_field IN ('owner', 'label', 'currency', 'amount', 'unrecorded'))
);

-- A pending top-up older than the reconciler's `stop_after` becomes `expired` (`ob_expire_pending_top_ups`): the
-- reconciler never reads it again, so the rows it scans are the ones in the window, however many were abandoned. An
-- expired top-up is not final: a webhook, or an operator's `reconcilePayment`, still settles it.
--
-- Two partial indexes over the pending rows, the only ones the reconciler reads:
--   `ob_top_up_pending_read`   the claim's order, `last_read_at NULLS FIRST, created_at`: the least recently read go
--                              first, so a backlog larger than a batch is covered before any row is read twice;
--   `ob_top_up_pending`        by creation, which the expiry walks from the oldest end.
CREATE INDEX ob_top_up_pending ON ob_top_up (created_at) WHERE state = 'pending';
CREATE INDEX ob_top_up_pending_read ON ob_top_up (last_read_at NULLS FIRST, created_at) WHERE state = 'pending';

-- A top-up's terms never change and it is never deleted. Its state moves only as follows:
--   pending    -> pending (a touch of `last_read_at`), credited, canceled, mismatched or expired;
--   expired    -> credited, canceled or mismatched (a late webhook or `reconcilePayment`);
--   mismatched -> pending, once, and only when the mismatch was `unrecorded` (a payment settled before its top-up was
--                 recorded) and the top-up recorded afterwards has the very terms read back: the row then gains its
--                 idempotency key and nothing else changes (`ob_record_top_up`). Any other mismatch is final.
-- credited and canceled are final. `plpgsql`: a trigger function, and it raises.
CREATE FUNCTION ob_top_up_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'ob_top_up: only a pending top-up may be read or closed; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    IF OLD.state = 'mismatched' AND OLD.mismatch_field = 'unrecorded' AND OLD.idem_key IS NULL
       AND NEW.state = 'pending' AND NEW.mismatch_field IS NULL AND NEW.idem_key IS NOT NULL
       AND (NEW.payment_id, NEW.owner, NEW.currency, NEW.label, NEW.amount, NEW.created_at)
           IS NOT DISTINCT FROM
           (OLD.payment_id, OLD.owner, OLD.currency, OLD.label, OLD.amount, OLD.created_at) THEN
        RETURN NEW;
    END IF;
    IF (NEW.payment_id, NEW.idem_key, NEW.owner, NEW.currency, NEW.label, NEW.amount, NEW.created_at)
          IS DISTINCT FROM
          (OLD.payment_id, OLD.idem_key, OLD.owner, OLD.currency, OLD.label, OLD.amount, OLD.created_at)
       OR OLD.state NOT IN ('pending', 'expired')
       OR (OLD.state = 'expired' AND NEW.state NOT IN ('credited', 'canceled', 'mismatched'))
       OR (NEW.state = 'pending' AND NEW.mismatch_field IS NOT NULL) THEN
        RAISE EXCEPTION 'ob_top_up: only a pending top-up may be read or closed; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER ob_top_up_guard BEFORE UPDATE OR DELETE ON ob_top_up
    FOR EACH ROW EXECUTE FUNCTION ob_top_up_guard();
CREATE TRIGGER ob_top_up_no_truncate BEFORE TRUNCATE ON ob_top_up
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();

-- Records a top-up, once-only by its payment id. Returns whether it was already recorded with these very terms (a
-- replay: the same idempotency key makes the provider answer the same payment). Other terms are OB011. The account
-- must exist (OB002), and the amount must be positive (OB009). `plpgsql`: it branches on a replay and raises.
--
-- One exception, so that a payer is never left uncredited: when a webhook settled the payment before this call
-- recorded it, under `payments.unrecorded: refuse`, a `mismatched` row of the field `unrecorded` holds the values read
-- back. If those values (owner, currency, label, amount) are the ones recorded now, the payment is ours after all: the
-- row is upgraded to a `pending` top-up carrying the key (answer `false`: this call recorded it), never read yet, and
-- the next settle or reconciliation pass credits it. If they differ the answer is OB011 and the row stays refused.
CREATE FUNCTION ob_record_top_up(p_payment_id text, p_key text, p_owner text, p_currency text, p_label text,
                                 p_amount bigint)
    RETURNS boolean
    LANGUAGE plpgsql AS $$
DECLARE
    v_inserted boolean;
    v_stored   ob_top_up%ROWTYPE;
BEGIN
    IF p_amount IS NULL OR p_amount <= 0 THEN
        RAISE EXCEPTION 'the amount of a top-up is positive' USING ERRCODE = 'OB009';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM ob_account a
                   WHERE a.owner = p_owner AND a.currency = p_currency AND a.label = p_label) THEN
        RAISE EXCEPTION 'no such account' USING ERRCODE = 'OB002';
    END IF;
    INSERT INTO ob_top_up (payment_id, idem_key, owner, currency, label, amount)
        VALUES (p_payment_id, p_key, p_owner, p_currency, p_label, p_amount)
        ON CONFLICT (payment_id) DO NOTHING
        RETURNING true INTO v_inserted;
    IF v_inserted THEN
        RETURN false;
    END IF;
    SELECT t.* INTO v_stored FROM ob_top_up t WHERE t.payment_id = p_payment_id FOR UPDATE;
    IF v_stored.state = 'mismatched' AND v_stored.mismatch_field = 'unrecorded' AND v_stored.idem_key IS NULL
       AND (v_stored.owner, v_stored.currency, v_stored.label, v_stored.amount)
           IS NOT DISTINCT FROM (p_owner, p_currency, p_label, p_amount) THEN
        UPDATE ob_top_up SET idem_key = p_key, state = 'pending', mismatch_field = NULL, last_read_at = NULL
            WHERE payment_id = p_payment_id;
        RETURN false;
    END IF;
    IF (v_stored.idem_key, v_stored.owner, v_stored.currency, v_stored.label, v_stored.amount)
       IS DISTINCT FROM (p_key, p_owner, p_currency, p_label, p_amount) THEN
        RAISE EXCEPTION 'the payment was already recorded as another top-up' USING ERRCODE = 'OB011';
    END IF;
    RETURN true;
END
$$;

-- Settles a payment read back from the provider: `p_status` is `pending`, `succeeded` or `canceled`, and the owner,
-- currency, label and amount are what the provider reports (the owner and the label are NULL when its metadata does
-- not carry them). `p_unrecorded` (`refuse` or `credit`) says what to do with a payment no top-up was recorded for.
--
-- Answers one row, and a mismatch is an ANSWER, not an error, so that the mark it leaves is committed:
--   `credited`   the recorded terms are booked once (a replay says so);
--   `canceled`   the top-up is closed;
--   `pending`    it is still unpaid (read time kept);
--   `not_ours`   no top-up, and no owner reference: not a payment of ours;
--   `mismatch`   `o_field` says which of `owner`, `label`, `currency`, `amount` differs, or `unrecorded`. A mismatch of
--                a recorded top-up is final: it is answered again, and never credited.
-- `o_marked` is true only in the call that marked the top-up mismatched (the first), so a caller counts and warns
-- once. Under `refuse` a succeeded payment with no top-up is recorded as a `mismatched` top-up of the field
-- `unrecorded` with the values read back, so an operator can list it and it is counted once. When the values read
-- back could not be a row (an id, a label or a currency outside its alphabet, an amount not above zero) there is
-- nothing to keep: it answers `mismatch` with `o_marked` true on every read.
-- The row is locked first, so a webhook and the reconciler settling at once book once. `plpgsql`: control flow.
CREATE FUNCTION ob_settle_top_up(p_payment_id text, p_status text, p_owner text, p_currency text, p_label text,
                                 p_amount bigint, p_unrecorded text)
    RETURNS TABLE (o_outcome text, o_field text, o_account_id text, o_txn_id uuid, o_replayed boolean,
                   o_repaid bigint, o_debt_added bigint, o_marked boolean)
    LANGUAGE plpgsql AS $$
DECLARE
    v_top      ob_top_up%ROWTYPE;
    v_field    text;
    v_inserted boolean;
    v_fits     boolean;
BEGIN
    IF p_status IS NULL OR p_status NOT IN ('pending', 'succeeded', 'canceled')
       OR p_unrecorded IS NULL OR p_unrecorded NOT IN ('refuse', 'credit') THEN
        RAISE EXCEPTION 'ob_settle_top_up: a status or a policy that is not one' USING ERRCODE = 'OB900';
    END IF;
    SELECT t.* INTO v_top FROM ob_top_up t WHERE t.payment_id = p_payment_id FOR UPDATE;

    IF NOT FOUND THEN
        IF p_status = 'canceled' THEN
            RETURN QUERY SELECT 'canceled', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        ELSIF p_status = 'pending' THEN
            RETURN QUERY SELECT 'pending', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        ELSIF p_owner IS NULL OR p_label IS NULL THEN
            RETURN QUERY SELECT 'not_ours', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        ELSIF p_unrecorded = 'refuse' THEN
            v_fits := ob_is_id(p_payment_id, 255) AND ob_is_id(p_owner, 255) AND p_amount > 0
                      AND ob_in_alphabet(p_currency, 16, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789')
                      AND ob_in_alphabet(p_label, 64, 'abcdefghijklmnopqrstuvwxyz0123456789_-');
            IF v_fits THEN
                INSERT INTO ob_top_up (payment_id, idem_key, owner, currency, label, amount, state, mismatch_field,
                                       last_read_at)
                    VALUES (p_payment_id, NULL, p_owner, p_currency, p_label, p_amount, 'mismatched', 'unrecorded',
                            clock_timestamp())
                    ON CONFLICT (payment_id) DO NOTHING
                    RETURNING true INTO v_inserted;
            END IF;
            -- Another call recorded it first: it is not this call's mark.
            RETURN QUERY SELECT 'mismatch', 'unrecorded', NULL::text, NULL::uuid, false, 0::bigint, 0::bigint,
                                (v_inserted IS NOT NULL OR NOT v_fits);
        ELSE
            RETURN QUERY SELECT 'credited', NULL::text, c.o_account_id, c.o_txn_id, c.o_replayed, c.o_repaid,
                                c.o_debt_added, false
                FROM ob_credit('payment', p_payment_id, NULL, p_owner, p_currency, p_label, p_amount, NULL) c;
        END IF;
        RETURN;
    END IF;

    IF v_top.state = 'mismatched' THEN
        RETURN QUERY SELECT 'mismatch', v_top.mismatch_field, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint,
                            false;
        RETURN;
    END IF;
    IF v_top.state = 'canceled' THEN
        RETURN QUERY SELECT 'canceled', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        RETURN;
    END IF;
    IF v_top.state = 'credited' THEN
        -- Booked already: the credit of the recorded terms answers again as a replay.
        RETURN QUERY SELECT 'credited', NULL::text, c.o_account_id, c.o_txn_id, c.o_replayed, c.o_repaid,
                            c.o_debt_added, false
            FROM ob_credit('payment', v_top.payment_id, NULL, v_top.owner, v_top.currency, v_top.label,
                           v_top.amount, NULL) c;
        RETURN;
    END IF;

    -- Pending, or expired (not read any more, but still settled by a webhook or an operator).
    IF p_status = 'canceled' THEN
        UPDATE ob_top_up SET state = 'canceled', last_read_at = clock_timestamp()
            WHERE payment_id = v_top.payment_id;
        RETURN QUERY SELECT 'canceled', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        RETURN;
    END IF;
    v_field := CASE
        WHEN p_owner IS DISTINCT FROM v_top.owner THEN 'owner'
        WHEN p_label IS DISTINCT FROM v_top.label THEN 'label'
        WHEN p_currency IS DISTINCT FROM v_top.currency THEN 'currency'
        WHEN p_amount IS DISTINCT FROM v_top.amount THEN 'amount'
    END;
    IF v_field IS NOT NULL THEN
        UPDATE ob_top_up SET state = 'mismatched', mismatch_field = v_field, last_read_at = clock_timestamp()
            WHERE payment_id = v_top.payment_id;
        RETURN QUERY SELECT 'mismatch', v_field, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, true;
        RETURN;
    END IF;
    IF p_status = 'pending' THEN
        IF v_top.state = 'pending' THEN
            UPDATE ob_top_up SET last_read_at = clock_timestamp() WHERE payment_id = v_top.payment_id;
        END IF;
        RETURN QUERY SELECT 'pending', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        RETURN;
    END IF;
    -- Succeeded and agreeing: book the recorded terms.
    RETURN QUERY SELECT 'credited', NULL::text, c.o_account_id, c.o_txn_id, c.o_replayed, c.o_repaid,
                        c.o_debt_added, false
        FROM ob_credit('payment', v_top.payment_id, NULL, v_top.owner, v_top.currency, v_top.label,
                       v_top.amount, NULL) c;
    UPDATE ob_top_up SET state = 'credited', last_read_at = clock_timestamp() WHERE payment_id = v_top.payment_id;
END
$$;

-- Moves the pending top-ups at least `p_stop_after_micros` old to `expired`, at most `p_limit` of them, the oldest
-- first (`ob_top_up_pending`), skipping rows another transaction holds; answers how many. It is bounded by the limit,
-- so a caller repeats it while it answers the limit. An expired top-up is not read by the reconciler any more and is
-- still settled by a webhook or `reconcilePayment`. Plain SQL: a SQL-standard body.
CREATE FUNCTION ob_expire_pending_top_ups(p_stop_after_micros bigint, p_limit integer)
    RETURNS bigint
    LANGUAGE sql
BEGIN ATOMIC
    WITH stale AS MATERIALIZED (
        SELECT d.payment_id FROM ob_top_up d
        WHERE d.state = 'pending'
          AND d.created_at <= now() - (make_interval(secs => (p_stop_after_micros / 1000000)::double precision)
                                       + (p_stop_after_micros % 1000000) * interval '1 microsecond')
        ORDER BY d.created_at
        LIMIT p_limit
        FOR UPDATE SKIP LOCKED),
    moved AS (
        UPDATE ob_top_up t SET state = 'expired'
        FROM stale s
        WHERE t.payment_id = s.payment_id
        RETURNING 1)
    SELECT count(*) FROM moved;
END;

-- Claims the pending top-ups that are due to be read back: at least `p_pending_after_micros` old and less than
-- `p_stop_after_micros` old, and never read, or last read at least `p_read_again_micros` ago; at most `p_limit`, the
-- least recently read first (never read, then by the last read, then the oldest), so a backlog larger than a batch is
-- covered before any row is read twice. The claim is the update of `last_read_at`, committed with the statement. The
-- rows are picked first, in a materialized CTE, with `FOR UPDATE SKIP LOCKED` under the LIMIT, so only `p_limit` rows
-- are ever locked and two callers at once never get the same top-up. The age tests are ranges on the stored columns
-- (sargable) over `state = 'pending'` rows, which `ob_expire_pending_top_ups` keeps to the window, and
-- `ob_top_up_pending_read` serves the ORDER BY. Plain SQL: a SQL-standard body.
CREATE FUNCTION ob_claim_pending_top_ups(p_pending_after_micros bigint, p_read_again_micros bigint,
                                         p_stop_after_micros bigint, p_limit integer)
    RETURNS TABLE (o_payment_id text)
    LANGUAGE sql
BEGIN ATOMIC
    WITH picked AS MATERIALIZED (
        SELECT d.payment_id FROM ob_top_up d
        WHERE d.state = 'pending'
          AND d.created_at <= now() - (make_interval(secs => (p_pending_after_micros / 1000000)::double precision)
                                       + (p_pending_after_micros % 1000000) * interval '1 microsecond')
          AND d.created_at > now() - (make_interval(secs => (p_stop_after_micros / 1000000)::double precision)
                                      + (p_stop_after_micros % 1000000) * interval '1 microsecond')
          AND (d.last_read_at IS NULL
               OR d.last_read_at <= now() - (make_interval(secs => (p_read_again_micros / 1000000)::double precision)
                                             + (p_read_again_micros % 1000000) * interval '1 microsecond'))
        ORDER BY d.last_read_at NULLS FIRST, d.created_at
        LIMIT p_limit
        FOR UPDATE SKIP LOCKED)
    UPDATE ob_top_up t SET last_read_at = clock_timestamp()
    FROM picked p
    WHERE t.payment_id = p.payment_id
    RETURNING t.payment_id;
END;
