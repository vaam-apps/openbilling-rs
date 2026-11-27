-- The ledger's rules, as functions (agent guide, rule 12). The Rust store calls them and maps what they raise.
--
-- A function runs inside its caller's transaction, so each call commits or rolls back whole; that is true of every
-- PostgreSQL function and is not what `BEGIN ATOMIC` adds. `BEGIN ATOMIC` and `RETURN` bodies are parsed when the
-- function is created and record their dependencies, so a table they use cannot be dropped or renamed under them
-- (CREATE FUNCTION, *sql_body*). `plpgsql` is used where a function needs control flow, a loop that sees its own
-- writes, or `RAISE` with a SQLSTATE; each such function says which.
--
-- Locks. Every write of an owner's money in one currency first locks that owner's `ob_balance` row, then touches
-- the owner's lots. A drain claims its event ids (outcome rows, in id order) before it takes any balance lock, and
-- takes its balance locks in (owner, currency) order before it draws. So no two transactions wait on each other in
-- a cycle (ADR 0001, point 13).
--
-- SQLSTATEs raised here (mapped in `openbilling-pg/src/sqlstate.rs`):
--   OB001 credit_conflict    OB002 unknown_account    OB003 unknown_currency   OB004 reversal_cap
--   OB005 incomplete_config  OB006 unknown_label      OB007 unknown_payment    OB008 account_conflict
--   OB009 invalid_amount     OB010 invalid_config
--   OB101 too_many_events    OB102 negative_amount    OB103 differing_duplicate
--   OB104 payload_conflict   OB105 too_old            OB106 too_far_ahead      OB107 unknown_currency (events)
--   OB108 overflow (events)
--   OB900 unbalanced transaction, OB901 a write to an immutable row, OB902 an outcome left pending: faults.
-- An OB1xx refusal names its events in DETAIL, separated by one space (an id holds no space).
-- 22003 (numeric_value_out_of_range) from a debt that would pass the largest amount is mapped to `overflow`.

-- ---------------------------------------------------------------------------------------------------------------
-- The payload hash (ADR 0001, point 3): SHA-256 over a JSON object with sorted keys, no insignificant whitespace,
-- the amount as an integer and the time in UTC to the microsecond. Plain SQL.

CREATE FUNCTION ob_payload_canonical(p_owner text, p_currency text, p_member text, p_amount bigint,
                                     p_occurred_at timestamptz) RETURNS text
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN '{"amount":' || p_amount::text
        || ',"currency":' || to_json(p_currency)::text
        || ',"member":' || to_json(p_member)::text
        || ',"occurred_at":"' || to_char(p_occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') || '"'
        || ',"owner":' || to_json(p_owner)::text
        || '}';

CREATE FUNCTION ob_payload_hash(p_owner text, p_currency text, p_member text, p_amount bigint,
                                p_occurred_at timestamptz) RETURNS bytea
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN sha256(convert_to(ob_payload_canonical(p_owner, p_currency, p_member, p_amount, p_occurred_at), 'UTF8'));

-- ---------------------------------------------------------------------------------------------------------------
-- Configuration. `plpgsql`: it raises OB010 and OB005.

CREATE FUNCTION ob_configure(p_currencies text[], p_labels text[], p_lot_kinds text[], p_soonest boolean[])
    RETURNS void
    LANGUAGE plpgsql AS $$
BEGIN
    IF coalesce(cardinality(p_currencies), 0) = 0
       OR coalesce(cardinality(p_labels), 0) = 0
       OR (SELECT count(DISTINCT c) FROM unnest(p_currencies) AS c) <> cardinality(p_currencies)
       OR (SELECT count(DISTINCT l) FROM unnest(p_labels) AS l) <> cardinality(p_labels)
       OR coalesce(cardinality(p_lot_kinds), 0) <> 4
       OR coalesce(cardinality(p_soonest), 0) <> 4
       OR (SELECT count(DISTINCT k) FROM unnest(p_lot_kinds) AS k
           WHERE k IN ('allowance', 'promotion', 'adjustment', 'payment')) <> 4 THEN
        RAISE EXCEPTION 'invalid ledger configuration' USING ERRCODE = 'OB010';
    END IF;
    IF EXISTS (SELECT 1 FROM ob_currency c WHERE c.code <> ALL (p_currencies))
       OR EXISTS (SELECT 1 FROM ob_label l WHERE l.label <> ALL (p_labels)) THEN
        RAISE EXCEPTION 'the configuration leaves out a currency or a label already configured'
            USING ERRCODE = 'OB005';
    END IF;
    INSERT INTO ob_currency (code) SELECT c FROM unnest(p_currencies) AS c ON CONFLICT (code) DO NOTHING;
    INSERT INTO ob_label (label, draw_rank)
        SELECT t.label, t.rank FROM unnest(p_labels) WITH ORDINALITY AS t(label, rank)
        ON CONFLICT (label) DO UPDATE SET draw_rank = excluded.draw_rank;
    UPDATE ob_lot_order o SET draw_rank = t.rank, soonest_expiry_first = t.soonest
        FROM unnest(p_lot_kinds, p_soonest) WITH ORDINALITY AS t(kind, soonest, rank)
        WHERE o.kind = t.kind;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Accounts. `plpgsql`: it raises OB003, OB006 and OB008, and turns the unique violation of (owner, currency, label)
-- into OB008.

CREATE FUNCTION ob_open_account(p_account_id text, p_owner text, p_currency text, p_label text) RETURNS boolean
    LANGUAGE plpgsql AS $$
DECLARE
    v_inserted boolean;
    v_owner    text;
    v_currency text;
    v_label    text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = p_currency) THEN
        RAISE EXCEPTION 'a currency the operator has not named' USING ERRCODE = 'OB003';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM ob_label l WHERE l.label = p_label) THEN
        RAISE EXCEPTION 'a label the operator has not configured' USING ERRCODE = 'OB006';
    END IF;
    BEGIN
        INSERT INTO ob_account AS a (account_id, owner, currency, label)
            VALUES (p_account_id, p_owner, p_currency, p_label)
            ON CONFLICT (account_id) DO SELECT
            RETURNING old.account_id IS NULL, new.owner, new.currency, new.label
            INTO v_inserted, v_owner, v_currency, v_label;
    EXCEPTION WHEN unique_violation THEN
        RAISE EXCEPTION 'the owner already has an account with this currency and label' USING ERRCODE = 'OB008';
    END;
    IF NOT v_inserted AND (v_owner, v_currency, v_label) IS DISTINCT FROM (p_owner, p_currency, p_label) THEN
        RAISE EXCEPTION 'the account id is taken with other terms' USING ERRCODE = 'OB008';
    END IF;
    INSERT INTO ob_balance (owner, currency) VALUES (p_owner, p_currency) ON CONFLICT (owner, currency) DO NOTHING;
    RETURN NOT v_inserted;
END
$$;

-- `plpgsql`: it raises OB009 and OB002.
CREATE FUNCTION ob_set_debt_limit(p_owner text, p_currency text, p_limit bigint) RETURNS void
    LANGUAGE plpgsql AS $$
BEGIN
    IF p_limit < 0 THEN
        RAISE EXCEPTION 'a debt limit cannot be negative' USING ERRCODE = 'OB009';
    END IF;
    UPDATE ob_balance b SET debt_limit = p_limit WHERE b.owner = p_owner AND b.currency = p_currency;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no account for this owner and currency' USING ERRCODE = 'OB002';
    END IF;
END
$$;

-- `plpgsql`: it raises OB003 and OB009, and either removes or writes.
CREATE FUNCTION ob_set_member_cap(p_owner text, p_currency text, p_member text, p_cap bigint) RETURNS void
    LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = p_currency) THEN
        RAISE EXCEPTION 'a currency the operator has not named' USING ERRCODE = 'OB003';
    END IF;
    IF p_cap < 0 THEN
        RAISE EXCEPTION 'a cap cannot be negative' USING ERRCODE = 'OB009';
    END IF;
    IF p_cap IS NULL THEN
        DELETE FROM ob_member_cap m WHERE m.owner = p_owner AND m.currency = p_currency AND m.member = p_member;
    ELSE
        INSERT INTO ob_member_cap (owner, currency, member, cap) VALUES (p_owner, p_currency, p_member, p_cap)
            ON CONFLICT (owner, currency, member) DO UPDATE SET cap = excluded.cap;
    END IF;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Drawing and debt. Plain SQL, so SQL-standard bodies. Each one runs under the caller's lock on the owner's balance
-- row, which is why it does not lock the lots it reads first: no other transaction moves them meanwhile, and the
-- lot's CHECK would refuse a draw past zero if one did.

-- Draws `p_amount` from the owner's drawable lots in the currency (of one account, when `p_account_id` is given),
-- in draw order: the account's label rank, then the lot kind's rank, then soonest expiry first (no expiry last) for
-- a kind ordered so, then oldest. Writes a lot entry per draw. Returns what it took.
CREATE FUNCTION ob_draw(p_txn uuid, p_owner text, p_currency text, p_account_id text, p_amount bigint)
    RETURNS bigint
    LANGUAGE sql
BEGIN ATOMIC
    WITH ordered AS (
        SELECT l.lot_id, l.account_id, l.remaining,
               sum(l.remaining) OVER (
                   ORDER BY lb.draw_rank, a.account_id, o.draw_rank,
                            CASE WHEN o.soonest_expiry_first THEN l.expires_at END NULLS LAST,
                            l.opened_at, l.lot_id
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) - l.remaining AS drawn_before
        FROM ob_lot l
        JOIN ob_account a ON a.account_id = l.account_id
        JOIN ob_label lb ON lb.label = a.label
        JOIN ob_lot_order o ON o.kind = l.kind
        WHERE a.owner = p_owner
          AND a.currency = p_currency
          AND (p_account_id IS NULL OR a.account_id = p_account_id)
          AND l.remaining > 0
          AND (l.not_before IS NULL OR l.not_before <= now())
          AND (l.expires_at IS NULL OR l.expires_at > now())
    ), plan AS (
        SELECT ordered.lot_id, ordered.account_id,
               least(ordered.remaining::numeric, p_amount - ordered.drawn_before)::bigint AS take
        FROM ordered
        WHERE ordered.drawn_before < p_amount
    ), drawn AS (
        UPDATE ob_lot SET remaining = ob_lot.remaining - plan.take
        FROM plan
        WHERE ob_lot.lot_id = plan.lot_id
        RETURNING ob_lot.lot_id
    ), entries AS (
        INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
        SELECT p_txn, p_currency, 'lot', p_owner, plan.account_id, plan.lot_id, -plan.take
        FROM plan
        RETURNING ob_entry.entry_id
    )
    SELECT coalesce(sum(plan.take), 0)::bigint FROM plan;
END;

-- Adds `p_amount` (when positive) to the owner's debt in the currency, with its debt entry. A debt past the
-- largest amount fails with 22003.
CREATE FUNCTION ob_add_debt(p_txn uuid, p_owner text, p_currency text, p_amount bigint) RETURNS bigint
    LANGUAGE sql
BEGIN ATOMIC
    WITH owed AS (
        UPDATE ob_balance SET debt = ob_balance.debt + p_amount
        WHERE ob_balance.owner = p_owner AND ob_balance.currency = p_currency AND p_amount > 0
        RETURNING ob_balance.debt
    ), entry AS (
        INSERT INTO ob_entry (txn_id, currency, book, owner, amount)
        SELECT p_txn, p_currency, 'debt', p_owner, -p_amount
        WHERE p_amount > 0
        RETURNING ob_entry.entry_id
    )
    SELECT coalesce((SELECT owed.debt FROM owed), 0);
END;

-- New credit repays debt first: takes the owner's debt, up to what the new lot holds, out of the lot, if the lot is
-- drawable now. Returns what it repaid.
CREATE FUNCTION ob_repay_from_lot(p_txn uuid, p_lot uuid) RETURNS bigint
    LANGUAGE sql
BEGIN ATOMIC
    WITH plan AS (
        SELECT l.lot_id, a.account_id, a.owner, a.currency, least(b.debt, l.remaining) AS repaid
        FROM ob_lot l
        JOIN ob_account a ON a.account_id = l.account_id
        JOIN ob_balance b ON b.owner = a.owner AND b.currency = a.currency
        WHERE l.lot_id = p_lot
          AND b.debt > 0
          AND l.remaining > 0
          AND (l.not_before IS NULL OR l.not_before <= now())
          AND (l.expires_at IS NULL OR l.expires_at > now())
    ), lot AS (
        UPDATE ob_lot SET remaining = ob_lot.remaining - plan.repaid
        FROM plan WHERE ob_lot.lot_id = plan.lot_id
        RETURNING ob_lot.lot_id
    ), balance AS (
        UPDATE ob_balance SET debt = ob_balance.debt - plan.repaid
        FROM plan WHERE ob_balance.owner = plan.owner AND ob_balance.currency = plan.currency
        RETURNING ob_balance.debt
    ), entries AS (
        INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
        SELECT p_txn, plan.currency, 'lot', plan.owner, plan.account_id, plan.lot_id, -plan.repaid FROM plan
        UNION ALL
        SELECT p_txn, plan.currency, 'debt', plan.owner, NULL, NULL, plan.repaid FROM plan
        RETURNING ob_entry.entry_id
    )
    SELECT coalesce((SELECT plan.repaid FROM plan), 0);
END;

-- ---------------------------------------------------------------------------------------------------------------
-- Credits (ADR 0002). `plpgsql`: it branches on the source and on a replay, and raises OB009, OB002 and OB001.
-- The account is given by id, or by owner, currency and label.

CREATE FUNCTION ob_credit(p_source text, p_credit_id text, p_account_id text, p_owner text, p_currency text,
                          p_label text, p_amount bigint, p_expires_micros bigint)
    RETURNS TABLE (o_account_id text, o_txn_id uuid, o_replayed boolean, o_repaid bigint, o_debt_added bigint)
    LANGUAGE plpgsql AS $$
DECLARE
    v_account         ob_account%ROWTYPE;
    v_expires         timestamptz := ob_ts(p_expires_micros);
    v_txn             uuid := uuidv7();
    v_inserted        boolean;
    v_stored_account  text;
    v_stored_amount   bigint;
    v_stored_expires  timestamptz;
    v_stored_txn      uuid;
    v_lot             uuid;
    v_taken           bigint;
BEGIN
    IF p_source IS NULL OR p_source NOT IN ('payment', 'promotion', 'adjustment')
       OR p_amount IS NULL OR p_amount = (-9223372036854775807 - 1)
       OR (p_source = 'adjustment' AND p_amount = 0)
       OR (p_source <> 'adjustment' AND p_amount <= 0)
       OR (p_amount < 0 AND p_expires_micros IS NOT NULL) THEN
        RAISE EXCEPTION 'the amount or expiry does not fit the credit''s source' USING ERRCODE = 'OB009';
    END IF;
    IF p_account_id IS NOT NULL THEN
        SELECT a.* INTO v_account FROM ob_account a WHERE a.account_id = p_account_id;
    ELSE
        SELECT a.* INTO v_account FROM ob_account a
        WHERE a.owner = p_owner AND a.currency = p_currency AND a.label = p_label;
    END IF;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no such account' USING ERRCODE = 'OB002';
    END IF;
    -- The owner's lock, before anything is read for the decision.
    PERFORM 1 FROM ob_balance b WHERE b.owner = v_account.owner AND b.currency = v_account.currency FOR UPDATE;

    -- Once-only by (source, id): insert, or return the row already there (ADR 0001, A1 and A2).
    INSERT INTO ob_credit AS c (source, credit_id, account_id, amount, expires_at, txn_id)
        VALUES (p_source, p_credit_id, v_account.account_id, p_amount, v_expires, v_txn)
        ON CONFLICT (source, credit_id) DO SELECT
        RETURNING old.credit_id IS NULL, new.account_id, new.amount, new.expires_at, new.txn_id
        INTO v_inserted, v_stored_account, v_stored_amount, v_stored_expires, v_stored_txn;
    IF NOT v_inserted THEN
        IF v_stored_account = v_account.account_id AND v_stored_amount = p_amount
           AND v_stored_expires IS NOT DISTINCT FROM v_expires THEN
            RETURN QUERY SELECT v_account.account_id, v_stored_txn, true, 0::bigint, 0::bigint;
            RETURN;
        END IF;
        RAISE EXCEPTION 'the credit id was already booked with other terms' USING ERRCODE = 'OB001';
    END IF;

    INSERT INTO ob_txn (txn_id, kind, currency) VALUES (v_txn, 'credit', v_account.currency);
    IF p_amount > 0 THEN
        INSERT INTO ob_lot (account_id, kind, source, credit_id, amount, remaining, expires_at, txn_id)
            VALUES (v_account.account_id, p_source, p_source, p_credit_id, p_amount, p_amount, v_expires, v_txn)
            RETURNING lot_id INTO v_lot;
        INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount) VALUES
            (v_txn, v_account.currency, 'lot', v_account.owner, v_account.account_id, v_lot, p_amount),
            (v_txn, v_account.currency, p_source, NULL, NULL, NULL, -p_amount);
        RETURN QUERY SELECT v_account.account_id, v_txn, false, ob_repay_from_lot(v_txn, v_lot), 0::bigint;
        RETURN;
    END IF;

    -- A negative adjustment takes credit from this account's lots in lot order; what they lack is debt.
    v_taken := ob_draw(v_txn, v_account.owner, v_account.currency, v_account.account_id, -p_amount);
    PERFORM ob_add_debt(v_txn, v_account.owner, v_account.currency, -p_amount - v_taken);
    INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
        VALUES (v_txn, v_account.currency, 'adjustment', NULL, NULL, NULL, -p_amount);
    RETURN QUERY SELECT v_account.account_id, v_txn, false, 0::bigint, -p_amount - v_taken;
END
$$;

-- Refunds and chargebacks. `plpgsql`: it branches on a replay and raises OB009, OB007 and OB001; the cap is the
-- trigger `ob_reversal_cap` (OB004).
CREATE FUNCTION ob_reverse(p_kind text, p_reversal_id text, p_payment_id text, p_amount bigint)
    RETURNS TABLE (o_account_id text, o_txn_id uuid, o_replayed boolean, o_repaid bigint, o_debt_added bigint)
    LANGUAGE plpgsql AS $$
DECLARE
    v_account        ob_account%ROWTYPE;
    v_txn            uuid := uuidv7();
    v_inserted       boolean;
    v_stored_payment text;
    v_stored_amount  bigint;
    v_stored_txn     uuid;
    v_lot            uuid;
    v_lot_remaining  bigint;
    v_taken          bigint;
BEGIN
    IF p_kind IS NULL OR p_kind NOT IN ('refund', 'chargeback') OR p_amount IS NULL OR p_amount <= 0 THEN
        RAISE EXCEPTION 'a reversal takes back a positive amount' USING ERRCODE = 'OB009';
    END IF;
    SELECT a.* INTO v_account
    FROM ob_credit c JOIN ob_account a ON a.account_id = c.account_id
    WHERE c.source = 'payment' AND c.credit_id = p_payment_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no such payment' USING ERRCODE = 'OB007';
    END IF;
    PERFORM 1 FROM ob_balance b WHERE b.owner = v_account.owner AND b.currency = v_account.currency FOR UPDATE;

    INSERT INTO ob_reversal AS r (kind, reversal_id, payment_id, amount, txn_id)
        VALUES (p_kind, p_reversal_id, p_payment_id, p_amount, v_txn)
        ON CONFLICT (kind, reversal_id) DO SELECT
        RETURNING old.reversal_id IS NULL, new.payment_id, new.amount, new.txn_id
        INTO v_inserted, v_stored_payment, v_stored_amount, v_stored_txn;
    IF NOT v_inserted THEN
        IF v_stored_payment = p_payment_id AND v_stored_amount = p_amount THEN
            RETURN QUERY SELECT v_account.account_id, v_stored_txn, true, 0::bigint, 0::bigint;
            RETURN;
        END IF;
        RAISE EXCEPTION 'the reversal id was already booked with other terms' USING ERRCODE = 'OB001';
    END IF;

    INSERT INTO ob_txn (txn_id, kind, currency) VALUES (v_txn, 'reversal', v_account.currency);
    SELECT l.lot_id, l.remaining INTO v_lot, v_lot_remaining
    FROM ob_lot l WHERE l.source = 'payment' AND l.credit_id = p_payment_id;
    v_taken := least(p_amount, v_lot_remaining);
    IF v_taken > 0 THEN
        UPDATE ob_lot l SET remaining = l.remaining - v_taken WHERE l.lot_id = v_lot;
        INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
            VALUES (v_txn, v_account.currency, 'lot', v_account.owner, v_account.account_id, v_lot, -v_taken);
    END IF;
    -- What was already spent is debt: the balance may go below zero.
    PERFORM ob_add_debt(v_txn, v_account.owner, v_account.currency, p_amount - v_taken);
    INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount)
        VALUES (v_txn, v_account.currency, 'payment', NULL, NULL, NULL, p_amount);
    RETURN QUERY SELECT v_account.account_id, v_txn, false, 0::bigint, p_amount - v_taken;
END
$$;

-- The monthly allowance: a lot spendable during its month only, once per account and month. `plpgsql`: it branches
-- on a replay and raises OB009, OB002 and OB001.
CREATE FUNCTION ob_grant_allowance(p_account_id text, p_year integer, p_month integer, p_amount bigint)
    RETURNS TABLE (o_account_id text, o_txn_id uuid, o_replayed boolean, o_repaid bigint, o_debt_added bigint)
    LANGUAGE plpgsql AS $$
DECLARE
    v_account       ob_account%ROWTYPE;
    v_month         date := make_date(p_year, p_month, 1);
    v_txn           uuid := uuidv7();
    v_inserted      boolean;
    v_stored_amount bigint;
    v_stored_txn    uuid;
    v_lot           uuid;
BEGIN
    IF p_amount IS NULL OR p_amount <= 0 THEN
        RAISE EXCEPTION 'an allowance is positive' USING ERRCODE = 'OB009';
    END IF;
    SELECT a.* INTO v_account FROM ob_account a WHERE a.account_id = p_account_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'no such account' USING ERRCODE = 'OB002';
    END IF;
    PERFORM 1 FROM ob_balance b WHERE b.owner = v_account.owner AND b.currency = v_account.currency FOR UPDATE;

    INSERT INTO ob_lot AS l (account_id, kind, allowance_month, amount, remaining, not_before, expires_at, txn_id)
        VALUES (p_account_id, 'allowance', v_month, p_amount, p_amount,
                v_month::timestamp AT TIME ZONE 'UTC',
                (v_month + interval '1 month') AT TIME ZONE 'UTC',
                v_txn)
        ON CONFLICT (account_id, allowance_month) DO SELECT
        RETURNING old.lot_id IS NULL, new.amount, new.txn_id, new.lot_id
        INTO v_inserted, v_stored_amount, v_stored_txn, v_lot;
    IF NOT v_inserted THEN
        IF v_stored_amount = p_amount THEN
            RETURN QUERY SELECT v_account.account_id, v_stored_txn, true, 0::bigint, 0::bigint;
            RETURN;
        END IF;
        RAISE EXCEPTION 'the month''s allowance was already granted for another amount' USING ERRCODE = 'OB001';
    END IF;

    INSERT INTO ob_txn (txn_id, kind, currency) VALUES (v_txn, 'allowance', v_account.currency);
    INSERT INTO ob_entry (txn_id, currency, book, owner, account_id, lot_id, amount) VALUES
        (v_txn, v_account.currency, 'lot', v_account.owner, v_account.account_id, v_lot, p_amount),
        (v_txn, v_account.currency, 'allowance', NULL, NULL, NULL, -p_amount);
    RETURN QUERY SELECT v_account.account_id, v_txn, false, ob_repay_from_lot(v_txn, v_lot), 0::bigint;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Intake (ADR 0001, points 1 to 9).

-- The batch as rows, with its position, its occurrence time and its payload hash. Plain SQL.
CREATE FUNCTION ob_intake_rows(p_event_ids text[], p_owners text[], p_currencies text[], p_members text[],
                               p_amounts bigint[], p_occurred_micros bigint[])
    RETURNS TABLE (i_position bigint, i_event_id text, i_owner text, i_currency text, i_member text,
                   i_amount bigint, i_occurred_at timestamptz, i_payload_hash bytea)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT t.pos, t.event_id, t.owner, t.currency, t.member, t.amount, ob_ts(t.occurred_micros),
           ob_payload_hash(t.owner, t.currency, t.member, t.amount, ob_ts(t.occurred_micros))
    FROM unnest(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros)
         WITH ORDINALITY AS t(event_id, owner, currency, member, amount, occurred_micros, pos);
END;

-- Takes a batch in, all or nothing. `plpgsql`: each rule raises its own SQLSTATE naming the events that broke it,
-- in batch order, and a raise rolls the whole call back. The rules, in order: at most 1000 events; no negative
-- amount; a repeated id only as an identical copy; per owner and currency, amounts that add up to one `Amount`; a
-- currency the operator named; the window, against the database's clock (point 8); no stored event of the same
-- id and day with another payload (point 2). Then one statement writes the rest: `INSERT … ON CONFLICT … DO SELECT
-- … RETURNING old…` (A1, A2), which takes no row lock (point 4). Returns, in batch order, `accepted` or `replayed`
-- (a later copy of an id in the batch is a replay).
CREATE FUNCTION ob_intake(p_event_ids text[], p_owners text[], p_currencies text[], p_members text[],
                          p_amounts bigint[], p_occurred_micros bigint[], p_window_days integer, p_skew_micros bigint)
    RETURNS TABLE (o_event_id text, o_answer text)
    LANGUAGE plpgsql AS $$
DECLARE
    v_named     text;
    v_ids       text[];
    v_inserted  boolean[];
    v_conflicts text;
BEGIN
    IF coalesce(cardinality(p_event_ids), 0) > 1000 THEN
        RAISE EXCEPTION 'the batch holds more than 1000 events' USING ERRCODE = 'OB101';
    END IF;
    IF p_window_days < 1 OR p_skew_micros < 0
       OR cardinality(p_owners) <> cardinality(p_event_ids) OR cardinality(p_currencies) <> cardinality(p_event_ids)
       OR cardinality(p_members) <> cardinality(p_event_ids) OR cardinality(p_amounts) <> cardinality(p_event_ids)
       OR cardinality(p_occurred_micros) <> cardinality(p_event_ids) THEN
        RAISE EXCEPTION 'malformed intake call' USING ERRCODE = '22023';
    END IF;
    IF coalesce(cardinality(p_event_ids), 0) = 0 THEN
        RETURN;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          WHERE r.i_amount < 0 GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a negative amount' USING ERRCODE = 'OB102', DETAIL = v_named;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          GROUP BY r.i_event_id HAVING count(DISTINCT r.i_payload_hash) > 1) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a repeat that differs from its first copy'
            USING ERRCODE = 'OB103', DETAIL = v_named;
    END IF;

    -- Every event of an owner and currency whose distinct events add up past the largest amount.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          WHERE (r.i_owner, r.i_currency) IN (
              SELECT d.i_owner, d.i_currency
              FROM (SELECT DISTINCT u.i_event_id, u.i_owner, u.i_currency, u.i_amount
                    FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts,
                                        p_occurred_micros) u) d
              GROUP BY d.i_owner, d.i_currency
              HAVING sum(d.i_amount::numeric) > 9223372036854775807)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: amounts that add up past the largest amount'
            USING ERRCODE = 'OB108', DETAIL = v_named;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          WHERE NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = r.i_currency)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a currency the operator has not named'
            USING ERRCODE = 'OB107', DETAIL = v_named;
    END IF;

    -- The window: no earlier than the start of the UTC day `p_window_days - 1` days ago.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          WHERE ob_utc_day(r.i_occurred_at) < ob_utc_day(now()) - (p_window_days - 1)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: an event before the window' USING ERRCODE = 'OB105', DETAIL = v_named;
    END IF;

    -- ...and no later than now plus the skew allowance.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
          WHERE r.i_occurred_at > now() + p_skew_micros * interval '1 microsecond'
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: an event too far in the future'
            USING ERRCODE = 'OB106', DETAIL = v_named;
    END IF;

    -- One statement writes the distinct events, in id order, or returns the rows already there.
    WITH batch AS (
        SELECT DISTINCT ON (r.i_event_id) r.*
        FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
        ORDER BY r.i_event_id, r.i_position
    ), written AS (
        INSERT INTO ob_usage_event AS e
            (event_id, occurred_on, occurred_at, owner, currency, member, amount, payload_hash)
        SELECT b.i_event_id, ob_utc_day(b.i_occurred_at), b.i_occurred_at, b.i_owner, b.i_currency, b.i_member,
               b.i_amount, b.i_payload_hash
        FROM batch b
        ORDER BY b.i_event_id
        ON CONFLICT (event_id, occurred_on) DO SELECT
        RETURNING new.event_id AS event_id, old.event_id IS NULL AS inserted, new.payload_hash AS stored_hash
    )
    SELECT array_agg(w.event_id), array_agg(w.inserted),
           string_agg(CASE WHEN w.stored_hash <> b.i_payload_hash THEN w.event_id END, ' ' ORDER BY b.i_position)
    INTO v_ids, v_inserted, v_conflicts
    FROM written w JOIN batch b ON b.i_event_id = w.event_id;
    IF v_conflicts IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: an id already recorded for its day with another payload'
            USING ERRCODE = 'OB104', DETAIL = v_conflicts;
    END IF;

    RETURN QUERY
        SELECT r.i_event_id,
               CASE WHEN w.inserted AND r.i_position = f.first_position THEN 'accepted' ELSE 'replayed' END
        FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) r
        JOIN unnest(v_ids, v_inserted) AS w(event_id, inserted) ON w.event_id = r.i_event_id
        JOIN (SELECT u.i_event_id, min(u.i_position) AS first_position
              FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros) u
              GROUP BY u.i_event_id) f ON f.i_event_id = r.i_event_id
        ORDER BY r.i_position;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- The drain (ADR 0001, points 11 to 15, 22 and 23). `plpgsql`: it loops over the batch, and each event's draw must
-- see the lots and the debt the one before it left.
--
-- 1. Take up to `p_limit` received events, oldest received first, skipping rows another drain holds.
-- 2. Claim each event id once, in id order, by inserting its outcome as pending; a row whose id already has an
--    outcome (another occurrence day, or earlier in this batch) is refused `id_reused` and moves no money (point 15).
-- 3. Lock the balance rows of the claimed events, in (owner, currency) order.
-- 4. Decide each claimed event, owner by owner: no account in the currency is `unknown_account`; a debit that would
--    take the owner's debt past its limit (or past the largest amount) is `limit`; otherwise draw it across the
--    owner's accounts and their lots and book the rest as debt, against revenue, in one ledger transaction.
-- 5. Move each event row to its outcome's state. Return one row per event taken, in the order taken.
CREATE FUNCTION ob_drain(p_limit integer)
    RETURNS TABLE (o_event_id text, o_occurred_day integer, o_state text, o_reason text, o_amount bigint)
    LANGUAGE plpgsql AS $$
DECLARE
    v_ids      text[];
    v_days     date[];
    v_won_ids  text[];
    v_won_days date[];
    v_event    record;
    v_debt     bigint;
    v_limit    bigint;
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
        INSERT INTO ob_event_outcome (event_id, occurred_on, occurred_at, owner, currency, member, amount, state)
        SELECT e.event_id, e.occurred_on, e.occurred_at, e.owner, e.currency, e.member, e.amount, 'pending'
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
        SELECT bl.debt, bl.debt_limit INTO v_debt, v_limit
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
        IF v_debt + v_unpaid > coalesce(v_limit, 9223372036854775807) THEN
            UPDATE ob_event_outcome o SET state = 'refused', reason = 'limit' WHERE o.event_id = v_event.event_id;
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
        SELECT e.event_id, ob_day(e.occurred_on), e.state, e.refusal, e.amount
        FROM unnest(v_ids, v_days) WITH ORDINALITY AS b(event_id, occurred_on, pos)
        JOIN ob_usage_event e ON e.event_id = b.event_id AND e.occurred_on = b.occurred_on
        ORDER BY b.pos;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Reads. Plain SQL: SQL-standard bodies. Time is given in microseconds since the epoch.

-- An owner's accounts in a currency, in draw order, with what their drawable lots hold at `p_at_micros`, and the
-- owner's debt. No row when the owner has no account in the currency.
CREATE FUNCTION ob_balance_of(p_owner text, p_currency text, p_at_micros bigint)
    RETURNS TABLE (o_account_id text, o_label text, o_available bigint, o_debt bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT a.account_id, a.label,
           ob_clamp(coalesce((SELECT sum(l.remaining) FROM ob_lot l
                              WHERE l.account_id = a.account_id AND l.remaining > 0
                                AND (l.not_before IS NULL OR l.not_before <= ob_ts(p_at_micros))
                                AND (l.expires_at IS NULL OR l.expires_at > ob_ts(p_at_micros))), 0)),
           b.debt
    FROM ob_account a
    JOIN ob_label lb ON lb.label = a.label
    JOIN ob_balance b ON b.owner = a.owner AND b.currency = a.currency
    WHERE a.owner = p_owner AND a.currency = p_currency
    ORDER BY lb.draw_rank, a.account_id;
END;

-- May `p_member` spend for the owner in the currency at `p_at_micros`? The balance (what the drawable lots hold,
-- less the debt) must be above zero; then, if the member has a cap, their applied usage in the UTC month of
-- `p_at_micros` must be under it. `o_refused` is `balance`, `member_cap` or null.
CREATE FUNCTION ob_allowed(p_owner text, p_currency text, p_member text, p_at_micros bigint)
    RETURNS TABLE (o_refused text, o_balance bigint, o_member_remaining bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT CASE WHEN x.balance <= 0 THEN 'balance'
                WHEN x.remaining IS NOT NULL AND x.remaining <= 0 THEN 'member_cap'
           END,
           ob_clamp(x.balance),
           ob_clamp(x.remaining)
    FROM (
        SELECT coalesce((SELECT sum(l.remaining) FROM ob_lot l JOIN ob_account a ON a.account_id = l.account_id
                         WHERE a.owner = p_owner AND a.currency = p_currency AND l.remaining > 0
                           AND (l.not_before IS NULL OR l.not_before <= ob_ts(p_at_micros))
                           AND (l.expires_at IS NULL OR l.expires_at > ob_ts(p_at_micros))), 0)
               - coalesce((SELECT b.debt FROM ob_balance b
                           WHERE b.owner = p_owner AND b.currency = p_currency), 0) AS balance,
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

-- An account's lots, in draw order, drawable or not. Months and days as days since the epoch, times as
-- microseconds.
CREATE FUNCTION ob_lots_of(p_account_id text)
    RETURNS TABLE (o_kind text, o_credit_id text, o_month_day integer, o_amount bigint, o_remaining bigint,
                   o_expires_micros bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT l.kind, l.credit_id, ob_day(l.allowance_month), l.amount, l.remaining, ob_micros(l.expires_at)
    FROM ob_lot l JOIN ob_lot_order o ON o.kind = l.kind
    WHERE l.account_id = p_account_id
    ORDER BY o.draw_rank, CASE WHEN o.soonest_expiry_first THEN l.expires_at END NULLS LAST, l.opened_at, l.lot_id;
END;

-- An event id's outcome, once decided.
CREATE FUNCTION ob_outcome_of(p_event_id text)
    RETURNS TABLE (o_occurred_day integer, o_state text, o_reason text, o_amount bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT ob_day(o.occurred_on), o.state, o.reason, o.amount
    FROM ob_event_outcome o
    WHERE o.event_id = p_event_id AND o.state <> 'pending';
END;

-- An owner's applied usage in a currency, per UTC day of occurrence, oldest first: the statement's daily lines.
CREATE FUNCTION ob_daily_usage(p_owner text, p_currency text)
    RETURNS TABLE (o_day integer, o_amount bigint)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT ob_day(o.occurred_on), ob_clamp(sum(o.amount))
    FROM ob_event_outcome o
    WHERE o.state = 'applied' AND o.owner = p_owner AND o.currency = p_currency
    GROUP BY o.occurred_on
    ORDER BY o.occurred_on;
END;
