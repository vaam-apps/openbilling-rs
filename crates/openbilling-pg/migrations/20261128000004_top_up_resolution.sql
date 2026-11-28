-- An operator resolves a mismatched top-up (ANR-1474, ADR 0003 amended 2026-11-28). A mismatch is sticky: nothing the
-- provider reports changes it. The operator decides, once, between two outcomes:
--   `credit_recorded`  the RECORDED terms (owner, label, currency, amount of the top-up) are credited once through
--                      `ob_credit`; the state becomes `resolved_credited`. Only a top-up that was recorded (it has an
--                      idempotency key) may be credited: the terms of an `unrecorded` one are what the provider read
--                      back, which the defence against a changed payment does not trust, so that one is only dismissed.
--   `dismiss`          the top-up is closed with no credit; the state becomes `resolved_dismissed`.
-- The choice is the state, and who made it is `resolved_by` (the operator's token subject; kept for audit in this
-- table only, never in a log or a metric) with `resolved_at`. Both resolved states are final. The mismatched field stays
-- on the row, so a resolved top-up still says what differed.

ALTER TABLE ob_top_up ADD COLUMN resolved_at timestamptz, ADD COLUMN resolved_by text;

ALTER TABLE ob_top_up DROP CONSTRAINT ob_top_up_state, DROP CONSTRAINT ob_top_up_mismatch;
ALTER TABLE ob_top_up
    ADD CONSTRAINT ob_top_up_state CHECK (state IN ('pending', 'credited', 'canceled', 'mismatched', 'expired',
                                                    'resolved_credited', 'resolved_dismissed')),
    ADD CONSTRAINT ob_top_up_mismatch CHECK (
        (state IN ('mismatched', 'resolved_credited', 'resolved_dismissed')) = (mismatch_field IS NOT NULL)),
    ADD CONSTRAINT ob_top_up_resolved CHECK (
        (state IN ('resolved_credited', 'resolved_dismissed')) = (resolved_at IS NOT NULL AND resolved_by IS NOT NULL)
        AND (resolved_at IS NULL) = (resolved_by IS NULL)),
    ADD CONSTRAINT ob_top_up_resolved_by CHECK (resolved_by IS NULL OR ob_is_id(resolved_by, 255)),
    ADD CONSTRAINT ob_top_up_resolved_credit_recorded CHECK (state <> 'resolved_credited' OR idem_key IS NOT NULL);

-- The guard of `20261128000002_top_ups.sql`, replaced: it also allows exactly one more move, `mismatched` to
-- `resolved_credited` or `resolved_dismissed`, with the terms, the key, the creation time, the mismatched field and the
-- last read unchanged, the resolution not set before and set now; `resolved_credited` only with a key. A resolved row is
-- never changed (the last branch's `OLD.state NOT IN ('pending', 'expired')`), and no other move sets a resolution.
-- `plpgsql`: a trigger function, and it raises.
CREATE OR REPLACE FUNCTION ob_top_up_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE' THEN
        RAISE EXCEPTION 'ob_top_up: only a pending top-up may be read or closed; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    IF OLD.state = 'mismatched' AND OLD.mismatch_field = 'unrecorded' AND OLD.idem_key IS NULL
       AND NEW.state = 'pending' AND NEW.mismatch_field IS NULL AND NEW.idem_key IS NOT NULL
       AND (NEW.payment_id, NEW.owner, NEW.currency, NEW.label, NEW.amount, NEW.created_at,
            NEW.resolved_at, NEW.resolved_by)
           IS NOT DISTINCT FROM
           (OLD.payment_id, OLD.owner, OLD.currency, OLD.label, OLD.amount, OLD.created_at,
            OLD.resolved_at, OLD.resolved_by) THEN
        RETURN NEW;
    END IF;
    IF OLD.state = 'mismatched' AND NEW.state IN ('resolved_credited', 'resolved_dismissed')
       AND (NEW.payment_id, NEW.idem_key, NEW.owner, NEW.currency, NEW.label, NEW.amount, NEW.created_at,
            NEW.mismatch_field, NEW.last_read_at)
           IS NOT DISTINCT FROM
           (OLD.payment_id, OLD.idem_key, OLD.owner, OLD.currency, OLD.label, OLD.amount, OLD.created_at,
            OLD.mismatch_field, OLD.last_read_at)
       AND OLD.resolved_at IS NULL AND OLD.resolved_by IS NULL
       AND NEW.resolved_at IS NOT NULL AND NEW.resolved_by IS NOT NULL
       AND (NEW.state = 'resolved_dismissed' OR OLD.idem_key IS NOT NULL) THEN
        RETURN NEW;
    END IF;
    IF (NEW.payment_id, NEW.idem_key, NEW.owner, NEW.currency, NEW.label, NEW.amount, NEW.created_at,
        NEW.resolved_at, NEW.resolved_by)
          IS DISTINCT FROM
          (OLD.payment_id, OLD.idem_key, OLD.owner, OLD.currency, OLD.label, OLD.amount, OLD.created_at,
           OLD.resolved_at, OLD.resolved_by)
       OR OLD.state NOT IN ('pending', 'expired')
       OR (OLD.state = 'expired' AND NEW.state NOT IN ('credited', 'canceled', 'mismatched'))
       OR NEW.state IN ('resolved_credited', 'resolved_dismissed')
       OR (NEW.state = 'pending' AND NEW.mismatch_field IS NOT NULL) THEN
        RAISE EXCEPTION 'ob_top_up: only a pending top-up may be read or closed; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

-- `ob_settle_top_up` of `20261128000002_top_ups.sql`, replaced with the same signature and body, and two more
-- branches for the resolved states: `resolved_credited` answers `credited` again (a replay of the credit of the
-- recorded terms, as `credited` does, whatever the provider reports now) and `resolved_dismissed` answers `mismatch`
-- with the row's field, never marked again, so a webhook or a reconciliation after the operator's choice changes
-- nothing. `plpgsql`: control flow.
CREATE OR REPLACE FUNCTION ob_settle_top_up(p_payment_id text, p_status text, p_owner text, p_currency text,
                                            p_label text, p_amount bigint, p_unrecorded text)
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

    IF v_top.state IN ('mismatched', 'resolved_dismissed') THEN
        RETURN QUERY SELECT 'mismatch', v_top.mismatch_field, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint,
                            false;
        RETURN;
    END IF;
    IF v_top.state = 'canceled' THEN
        RETURN QUERY SELECT 'canceled', NULL::text, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint, false;
        RETURN;
    END IF;
    IF v_top.state IN ('credited', 'resolved_credited') THEN
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

-- Resolves a mismatched top-up: `p_outcome` is `credit_recorded` or `dismiss`, `p_by` the operator's subject. Once-only
-- by the row, which is locked first: the same outcome again answers the first one (for a credit, the replay of its
-- credit); the other outcome after one was taken is OB013; a top-up that is not mismatched is OB012; no such payment is
-- OB007; `credit_recorded` on a mismatch no top-up was recorded for (no idempotency key: `unrecorded`) is OB014, such
-- a row may only be dismissed. The credit books the RECORDED terms (never what the provider reports) through
-- `ob_credit`, in the same transaction as the state change. Answers one row: the outcome, the mismatched field, and the
-- credit's account, transaction, replay, repaid and debt added (NULL, false and zero for a dismissal). `plpgsql`:
-- branches and `RAISE` with a SQLSTATE.
CREATE FUNCTION ob_resolve_top_up(p_payment_id text, p_outcome text, p_by text)
    RETURNS TABLE (o_outcome text, o_field text, o_account_id text, o_txn_id uuid, o_replayed boolean,
                   o_repaid bigint, o_debt_added bigint)
    LANGUAGE plpgsql AS $$
DECLARE
    v_top ob_top_up%ROWTYPE;
BEGIN
    IF p_outcome IS NULL OR p_outcome NOT IN ('credit_recorded', 'dismiss') OR p_by IS NULL
       OR NOT ob_is_id(p_by, 255) THEN
        RAISE EXCEPTION 'ob_resolve_top_up: an outcome or an operator that is not one' USING ERRCODE = 'OB900';
    END IF;
    SELECT t.* INTO v_top FROM ob_top_up t WHERE t.payment_id = p_payment_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no such payment' USING ERRCODE = 'OB007';
    END IF;

    IF v_top.state IN ('resolved_credited', 'resolved_dismissed') THEN
        IF (v_top.state = 'resolved_credited') <> (p_outcome = 'credit_recorded') THEN
            RAISE EXCEPTION 'the top-up was resolved the other way' USING ERRCODE = 'OB013';
        END IF;
        IF v_top.state = 'resolved_credited' THEN
            RETURN QUERY SELECT 'credit_recorded', v_top.mismatch_field, c.o_account_id, c.o_txn_id, c.o_replayed,
                                c.o_repaid, c.o_debt_added
                FROM ob_credit('payment', v_top.payment_id, NULL, v_top.owner, v_top.currency, v_top.label,
                               v_top.amount, NULL) c;
        ELSE
            RETURN QUERY SELECT 'dismiss', v_top.mismatch_field, NULL::text, NULL::uuid, true, 0::bigint,
                                0::bigint;
        END IF;
        RETURN;
    END IF;
    IF v_top.state <> 'mismatched' THEN
        RAISE EXCEPTION 'the top-up is not mismatched' USING ERRCODE = 'OB012';
    END IF;

    IF p_outcome = 'dismiss' THEN
        UPDATE ob_top_up SET state = 'resolved_dismissed', resolved_at = clock_timestamp(), resolved_by = p_by
            WHERE payment_id = v_top.payment_id;
        RETURN QUERY SELECT 'dismiss', v_top.mismatch_field, NULL::text, NULL::uuid, false, 0::bigint, 0::bigint;
        RETURN;
    END IF;
    IF v_top.idem_key IS NULL THEN
        RAISE EXCEPTION 'no top-up was recorded for the payment: only a dismissal' USING ERRCODE = 'OB014';
    END IF;
    RETURN QUERY SELECT 'credit_recorded', v_top.mismatch_field, c.o_account_id, c.o_txn_id, c.o_replayed,
                        c.o_repaid, c.o_debt_added
        FROM ob_credit('payment', v_top.payment_id, NULL, v_top.owner, v_top.currency, v_top.label,
                       v_top.amount, NULL) c;
    UPDATE ob_top_up SET state = 'resolved_credited', resolved_at = clock_timestamp(), resolved_by = p_by
        WHERE payment_id = v_top.payment_id;
END
$$;
