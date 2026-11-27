-- The double-entry ledger (ADR 0001, points 21 to 25; ADR 0002).
--
-- A ledger transaction has entries in one currency that sum to zero. An entry is on a book:
--   lot        credit held in one lot of one account (positive when credit comes in, negative when it is drawn);
--   debt       what an owner owes in a currency (negative when debt is run up, positive when it is repaid);
--   revenue    the counter-book of usage;
--   payment, promotion, adjustment, allowance
--              the counter-books of credit coming in or, for a reversal or a negative adjustment, going out.
-- Counter-books carry no balance row (ADR 0001, point 24): their balance is the sum of their entries.

CREATE TABLE ob_txn (
    txn_id     uuid PRIMARY KEY DEFAULT uuidv7(),
    kind       text NOT NULL,
    currency   text NOT NULL REFERENCES ob_currency (code),
    -- The event a usage transaction books, under UNIQUE: defence in depth for once-only (ADR 0001, point 14). The
    -- foreign key to the outcome comes with the queue's migration.
    event_id   text UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ob_txn_kind CHECK (kind IN ('usage', 'credit', 'reversal', 'allowance')),
    CONSTRAINT ob_txn_event CHECK ((kind = 'usage') = (event_id IS NOT NULL)),
    CONSTRAINT ob_txn_id_currency_key UNIQUE (txn_id, currency)
);

-- Credits (ADR 0002): once-only by source and the caller's id, kept for good.
CREATE TABLE ob_credit (
    source     text NOT NULL,
    credit_id  text NOT NULL,
    account_id text NOT NULL REFERENCES ob_account (account_id),
    amount     bigint NOT NULL,
    expires_at timestamptz,
    -- Written before its transaction, so a replay that finds the row books no transaction at all: deferred.
    txn_id     uuid NOT NULL REFERENCES ob_txn (txn_id) DEFERRABLE INITIALLY DEFERRED,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (source, credit_id),
    CONSTRAINT ob_credit_source CHECK (source IN ('payment', 'promotion', 'adjustment')),
    CONSTRAINT ob_credit_id CHECK (ob_is_id(credit_id, 128)),
    -- Payments and promotions add credit; an adjustment adds or takes it, never zero. `i64::MIN` has no negation.
    CONSTRAINT ob_credit_sign CHECK (
        CASE source WHEN 'adjustment' THEN amount <> 0 ELSE amount > 0 END
        AND amount <> (-9223372036854775807 - 1)),
    CONSTRAINT ob_credit_expiry CHECK (amount > 0 OR expires_at IS NULL)
);

-- Lots: parcels of credit in one account, drawn by the debit. `remaining` is the projection of the lot's entries,
-- written in the same transaction as each of them.
CREATE TABLE ob_lot (
    lot_id          uuid PRIMARY KEY DEFAULT uuidv7(),
    account_id      text NOT NULL REFERENCES ob_account (account_id),
    kind            text NOT NULL,
    source          text,
    credit_id       text,
    allowance_month date,
    amount          bigint NOT NULL,
    remaining       bigint NOT NULL,
    not_before      timestamptz,
    expires_at      timestamptz,
    -- The clock, not the transaction's start, so lots opened in one transaction keep their order.
    opened_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
    txn_id          uuid NOT NULL REFERENCES ob_txn (txn_id) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (source, credit_id) REFERENCES ob_credit (source, credit_id),
    CONSTRAINT ob_lot_kind CHECK (kind IN ('allowance', 'promotion', 'adjustment', 'payment')),
    -- An allowance lot names its month and no credit; any other lot was opened by a credit of its own kind.
    CONSTRAINT ob_lot_opened_by CHECK (
        CASE kind
            WHEN 'allowance' THEN source IS NULL AND credit_id IS NULL AND allowance_month IS NOT NULL
                                  AND extract(day FROM allowance_month) = 1
            ELSE source = kind AND credit_id IS NOT NULL AND allowance_month IS NULL
        END),
    CONSTRAINT ob_lot_amounts CHECK (amount > 0 AND remaining BETWEEN 0 AND amount),
    CONSTRAINT ob_lot_credit_key UNIQUE (source, credit_id),
    -- One allowance per account and month, whoever grants it. Other lots have no month, and NULLs are distinct.
    CONSTRAINT ob_lot_allowance_key UNIQUE (account_id, allowance_month)
);

CREATE INDEX ob_lot_open ON ob_lot (account_id) WHERE remaining > 0;

-- Refunds and chargebacks of a payment: once-only by kind and the caller's id.
CREATE TABLE ob_reversal (
    kind        text NOT NULL,
    reversal_id text NOT NULL,
    source      text NOT NULL DEFAULT 'payment',
    payment_id  text NOT NULL,
    amount      bigint NOT NULL,
    txn_id      uuid NOT NULL REFERENCES ob_txn (txn_id) DEFERRABLE INITIALLY DEFERRED,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, reversal_id),
    FOREIGN KEY (source, payment_id) REFERENCES ob_credit (source, credit_id),
    CONSTRAINT ob_reversal_kind CHECK (kind IN ('refund', 'chargeback')),
    -- Only a payment carries the reversal cap, and only a payment can be reversed.
    CONSTRAINT ob_reversal_of_a_payment CHECK (source = 'payment'),
    CONSTRAINT ob_reversal_id CHECK (ob_is_id(reversal_id, 128)),
    CONSTRAINT ob_reversal_amount CHECK (amount > 0)
);

CREATE INDEX ob_reversal_payment ON ob_reversal (payment_id);

CREATE TABLE ob_entry (
    entry_id   uuid PRIMARY KEY DEFAULT uuidv7(),
    txn_id     uuid NOT NULL,
    currency   text NOT NULL,
    book       text NOT NULL,
    owner      text,
    account_id text REFERENCES ob_account (account_id),
    lot_id     uuid REFERENCES ob_lot (lot_id),
    amount     bigint NOT NULL,
    -- An entry is in its transaction's currency.
    FOREIGN KEY (txn_id, currency) REFERENCES ob_txn (txn_id, currency),
    CONSTRAINT ob_entry_book CHECK (
        book IN ('lot', 'debt', 'revenue', 'payment', 'promotion', 'adjustment', 'allowance')),
    CONSTRAINT ob_entry_amount CHECK (amount <> 0 AND amount <> (-9223372036854775807 - 1)),
    CONSTRAINT ob_entry_lot CHECK ((book = 'lot') = (lot_id IS NOT NULL) AND (book = 'lot') = (account_id IS NOT NULL)),
    CONSTRAINT ob_entry_owner CHECK ((book IN ('lot', 'debt')) = (owner IS NOT NULL))
);

CREATE INDEX ob_entry_txn ON ob_entry (txn_id);
CREATE INDEX ob_entry_lot ON ob_entry (lot_id) WHERE lot_id IS NOT NULL;
CREATE INDEX ob_entry_debt ON ob_entry (owner, currency) WHERE book = 'debt';

-- ---------------------------------------------------------------------------------------------------------------
-- Invariants that a CHECK cannot say. Trigger functions are `plpgsql`: a trigger function cannot be `LANGUAGE sql`,
-- and each one raises a SQLSTATE of its own.

-- The ledger is append-only. A mistake is corrected by a new entry (ADR 0002: an adjustment).
CREATE FUNCTION ob_refuse_change() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: % refused', TG_TABLE_NAME, TG_OP USING ERRCODE = 'OB901';
END
$$;

CREATE TRIGGER ob_txn_append_only BEFORE UPDATE OR DELETE ON ob_txn
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_txn_no_truncate BEFORE TRUNCATE ON ob_txn
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_entry_append_only BEFORE UPDATE OR DELETE ON ob_entry
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_entry_no_truncate BEFORE TRUNCATE ON ob_entry
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_credit_append_only BEFORE UPDATE OR DELETE ON ob_credit
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_credit_no_truncate BEFORE TRUNCATE ON ob_credit
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_reversal_append_only BEFORE UPDATE OR DELETE ON ob_reversal
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_reversal_no_truncate BEFORE TRUNCATE ON ob_reversal
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();

-- A lot is never deleted, and only its `remaining` ever changes.
CREATE FUNCTION ob_lot_only_remaining_moves() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE'
       OR (NEW.lot_id, NEW.account_id, NEW.kind, NEW.source, NEW.credit_id, NEW.allowance_month, NEW.amount,
           NEW.not_before, NEW.expires_at, NEW.opened_at, NEW.txn_id)
          IS DISTINCT FROM
          (OLD.lot_id, OLD.account_id, OLD.kind, OLD.source, OLD.credit_id, OLD.allowance_month, OLD.amount,
           OLD.not_before, OLD.expires_at, OLD.opened_at, OLD.txn_id) THEN
        RAISE EXCEPTION 'ob_lot: only remaining may change; % refused', TG_OP USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER ob_lot_guard BEFORE UPDATE OR DELETE ON ob_lot
    FOR EACH ROW EXECUTE FUNCTION ob_lot_only_remaining_moves();
CREATE TRIGGER ob_lot_no_truncate BEFORE TRUNCATE ON ob_lot
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();

-- A transaction balances, and has entries: checked at commit, after all of them are written (ADR 0001, point 21).
CREATE FUNCTION ob_check_txn_balanced() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF coalesce((SELECT sum(e.amount) FROM ob_entry e WHERE e.txn_id = NEW.txn_id), 1) <> 0 THEN
        RAISE EXCEPTION 'ledger transaction % does not balance or has no entry', NEW.txn_id
            USING ERRCODE = 'OB900';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER ob_txn_balanced AFTER INSERT ON ob_txn
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ob_check_txn_balanced();
CREATE CONSTRAINT TRIGGER ob_entry_balanced AFTER INSERT ON ob_entry
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ob_check_txn_balanced();

-- The reversal cap: refunds and chargebacks of a payment, together, never take back more than it. Checked when the
-- reversal is inserted, under the lock `ob_reverse` takes on the owner's balance row, so two reversals cannot both
-- pass it. The one place the cap is written.
CREATE FUNCTION ob_check_reversal_cap() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT sum(r.amount) FROM ob_reversal r WHERE r.payment_id = NEW.payment_id)
       > (SELECT c.amount FROM ob_credit c WHERE c.source = 'payment' AND c.credit_id = NEW.payment_id) THEN
        RAISE EXCEPTION 'the reversal would take back more than its payment' USING ERRCODE = 'OB004';
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER ob_reversal_cap AFTER INSERT ON ob_reversal
    FOR EACH ROW EXECUTE FUNCTION ob_check_reversal_cap();

-- Projections that drifted from their entries. Both must always be empty; the tests read them.
CREATE VIEW ob_lot_drift AS
    SELECT l.lot_id, l.remaining, coalesce(sum(e.amount), 0) AS from_entries
    FROM ob_lot l LEFT JOIN ob_entry e ON e.lot_id = l.lot_id
    GROUP BY l.lot_id, l.remaining
    HAVING l.remaining <> coalesce(sum(e.amount), 0);

CREATE VIEW ob_debt_drift AS
    SELECT b.owner, b.currency, b.debt, coalesce(-sum(e.amount), 0) AS from_entries
    FROM ob_balance b
    LEFT JOIN ob_entry e ON e.book = 'debt' AND e.owner = b.owner AND e.currency = b.currency
    GROUP BY b.owner, b.currency, b.debt
    HAVING b.debt <> coalesce(-sum(e.amount), 0);
