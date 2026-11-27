-- Configuration and accounts (ADR 0002). Hand-written: ADR 0001 says why CrateStack does not model these tables.
--
-- Every name is prefixed `ob_`. Every rule of the ledger lives in these migrations (agent guide, rule 12); the Rust
-- store only calls the functions. A function whose logic is plain SQL has a SQL-standard body (`RETURN …` or
-- `BEGIN ATOMIC … END`), parsed when it is created, with its dependencies recorded. A function that needs control
-- flow or `RAISE` with a SQLSTATE is `LANGUAGE plpgsql`, and says why.
--
-- The ledger's own SQLSTATEs are class `OB` (the implementation-defined range, Appendix A of the PostgreSQL
-- documentation); `openbilling-pg`'s `sqlstate.rs` maps each one to a refusal or a class, in one table.

-- ---------------------------------------------------------------------------------------------------------------
-- Helpers. Plain SQL: SQL-standard bodies.

-- Whether `p_value` is 1 to `p_max` characters, each one in `p_alphabet`. `ltrim` removes the alphabet's characters
-- from the left; nothing may be left. No regular expression, so no range depends on a collation.
CREATE FUNCTION ob_in_alphabet(p_value text, p_max integer, p_alphabet text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN char_length(p_value) BETWEEN 1 AND p_max AND ltrim(p_value, p_alphabet) = '';

-- A caller-given id: printable ASCII, `!` to `~` (openbilling-core's `EventId` and its siblings).
CREATE FUNCTION ob_is_id(p_value text, p_max integer) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN ob_in_alphabet(p_value, p_max,
        '!"#$%&''()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\]^_`abcdefghijklmnopqrstuvwxyz{|}~');

-- Microseconds since the Unix epoch to a timestamp, and back: how the Rust store passes time, so no time library is
-- needed. Split into whole seconds and microseconds so each part is exact in the float an interval product takes.
CREATE FUNCTION ob_ts(p_micros bigint) RETURNS timestamptz
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN timestamptz '1970-01-01 00:00:00+00'
        + make_interval(secs => (p_micros / 1000000)::double precision)
        + (p_micros % 1000000) * interval '1 microsecond';

CREATE FUNCTION ob_micros(p_at timestamptz) RETURNS bigint
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN (extract(epoch FROM p_at) * 1000000)::bigint;

-- A date as days since 1970-01-01, the Rust `UtcDay`.
CREATE FUNCTION ob_day(p_day date) RETURNS integer
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN p_day - date '1970-01-01';

-- The UTC day of an instant.
CREATE FUNCTION ob_utc_day(p_at timestamptz) RETURNS date
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN (p_at AT TIME ZONE 'UTC')::date;

-- A bigint sum clamped to the range of an `Amount`, for reads that add many amounts up.
CREATE FUNCTION ob_clamp(p_value numeric) RETURNS bigint
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN greatest(least(p_value, 9223372036854775807), -9223372036854775807)::bigint;

-- ---------------------------------------------------------------------------------------------------------------
-- Configuration.

-- The currencies the operator names. The ledger never converts between them.
CREATE TABLE ob_currency (
    code text PRIMARY KEY,
    CONSTRAINT ob_currency_code CHECK (ob_in_alphabet(code, 16, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789'))
);

-- The account labels, and the order the debit draws an owner's accounts of one currency in: rank 1 first.
CREATE TABLE ob_label (
    label     text PRIMARY KEY,
    draw_rank integer NOT NULL,
    CONSTRAINT ob_label_name CHECK (ob_in_alphabet(label, 64, 'abcdefghijklmnopqrstuvwxyz0123456789_-')),
    -- Deferred, so a new configuration can swap two ranks in one statement.
    CONSTRAINT ob_label_draw_rank_key UNIQUE (draw_rank) DEFERRABLE INITIALLY DEFERRED
);

-- The order lots are drawn in within an account: by kind, then, for a kind whose lots go soonest expiry first, by
-- expiry with no expiry last, then oldest first.
CREATE TABLE ob_lot_order (
    kind                 text PRIMARY KEY,
    draw_rank            integer NOT NULL,
    soonest_expiry_first boolean NOT NULL,
    CONSTRAINT ob_lot_order_kind CHECK (kind IN ('allowance', 'promotion', 'adjustment', 'payment')),
    CONSTRAINT ob_lot_order_draw_rank_key UNIQUE (draw_rank) DEFERRABLE INITIALLY DEFERRED
);

-- The example order (openbilling-core's `LotOrder::default`).
INSERT INTO ob_lot_order (kind, draw_rank, soonest_expiry_first) VALUES
    ('allowance', 1, true),
    ('promotion', 2, true),
    ('adjustment', 3, false),
    ('payment', 4, false);

-- ---------------------------------------------------------------------------------------------------------------
-- Accounts (ADR 0002): an owner holds several, each with one currency and one label.

CREATE TABLE ob_account (
    account_id text PRIMARY KEY,
    owner      text NOT NULL,
    currency   text NOT NULL REFERENCES ob_currency (code),
    label      text NOT NULL REFERENCES ob_label (label),
    opened_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ob_account_id CHECK (ob_is_id(account_id, 128)),
    CONSTRAINT ob_account_owner CHECK (ob_is_id(owner, 255)),
    CONSTRAINT ob_account_owner_currency_label_key UNIQUE (owner, currency, label)
);

-- One row per owner and currency that has an account: the row every write of that owner's money in that currency
-- locks first (ADR 0001, point 13), and where its debt is kept. Its debt is what found no credit; new credit
-- repays it first.
CREATE TABLE ob_balance (
    owner      text NOT NULL,
    currency   text NOT NULL REFERENCES ob_currency (code),
    debt       bigint NOT NULL DEFAULT 0,
    debt_limit bigint,
    PRIMARY KEY (owner, currency),
    CONSTRAINT ob_balance_debt CHECK (debt >= 0),
    CONSTRAINT ob_balance_debt_limit CHECK (debt_limit >= 0)
);

-- A member's monthly cap for an owner in a currency (a calendar month, UTC). Read by `ob_allowed`.
CREATE TABLE ob_member_cap (
    owner    text NOT NULL,
    currency text NOT NULL REFERENCES ob_currency (code),
    member   text NOT NULL,
    cap      bigint NOT NULL,
    PRIMARY KEY (owner, currency, member),
    CONSTRAINT ob_member_cap_owner CHECK (ob_is_id(owner, 255)),
    CONSTRAINT ob_member_cap_member CHECK (ob_is_id(member, 128)),
    CONSTRAINT ob_member_cap_cap CHECK (cap >= 0)
);
