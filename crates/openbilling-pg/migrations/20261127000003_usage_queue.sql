-- The usage queue (ADR 0001, points 1 to 17). The event table is the queue and the once-only record at once.

-- One row per event and occurrence day, partitioned by that day (point 7). No default partition (point 10): an event
-- for a day without one fails, and the store answers "unavailable".
CREATE TABLE ob_usage_event (
    event_id     text NOT NULL,
    occurred_on  date NOT NULL,
    occurred_at  timestamptz NOT NULL,
    owner        text NOT NULL,
    currency     text NOT NULL REFERENCES ob_currency (code),
    member       text NOT NULL,
    amount       bigint NOT NULL,
    payload_hash bytea NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    state        text NOT NULL DEFAULT 'received',
    refusal      text,
    PRIMARY KEY (event_id, occurred_on),
    -- A plain column held to the occurrence time: a generated column cannot be a partition key.
    CONSTRAINT ob_usage_event_day CHECK (occurred_on = ob_utc_day(occurred_at)),
    CONSTRAINT ob_usage_event_ids CHECK (ob_is_id(event_id, 128) AND ob_is_id(owner, 255) AND ob_is_id(member, 128)),
    CONSTRAINT ob_usage_event_amount CHECK (amount >= 0),
    CONSTRAINT ob_usage_event_hash CHECK (octet_length(payload_hash) = 32),
    CONSTRAINT ob_usage_event_state CHECK (state IN ('received', 'applied', 'refused')),
    CONSTRAINT ob_usage_event_refusal CHECK (
        (state = 'refused') = (refusal IS NOT NULL)
        AND (refusal IS NULL OR refusal IN ('limit', 'no_price', 'unknown_account', 'id_reused')))
) PARTITION BY RANGE (occurred_on);

-- The drain's index, and the lag's (points 11 and 26).
CREATE INDEX ob_usage_event_received ON ob_usage_event (received_at, event_id) WHERE state = 'received';

-- An event row changes once, from received to its final state; nothing else of it ever changes. A trigger function:
-- `plpgsql`.
CREATE FUNCTION ob_usage_event_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE'
       OR OLD.state <> 'received'
       OR NEW.state = 'received'
       OR (NEW.event_id, NEW.occurred_on, NEW.occurred_at, NEW.owner, NEW.currency, NEW.member, NEW.amount,
           NEW.payload_hash, NEW.received_at)
          IS DISTINCT FROM
          (OLD.event_id, OLD.occurred_on, OLD.occurred_at, OLD.owner, OLD.currency, OLD.member, OLD.amount,
           OLD.payload_hash, OLD.received_at) THEN
        RAISE EXCEPTION 'ob_usage_event: only a received event moves to its final state; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

-- On the partitioned table, so every partition gets a clone (CREATE TRIGGER, *partitioned tables*). Dropping a whole
-- partition (point 10) is DDL and fires no row trigger.
CREATE TRIGGER ob_usage_event_guard BEFORE UPDATE OR DELETE ON ob_usage_event
    FOR EACH ROW EXECUTE FUNCTION ob_usage_event_guard();

-- The durable outcome of each event id (point 14), kept for good. A drain claims an id by inserting its outcome as
-- `pending` before it takes any balance lock, then decides it; at commit, no outcome is left pending.
CREATE TABLE ob_event_outcome (
    event_id    text PRIMARY KEY,
    occurred_on date NOT NULL,
    occurred_at timestamptz NOT NULL,
    owner       text NOT NULL,
    currency    text NOT NULL REFERENCES ob_currency (code),
    member      text NOT NULL,
    amount      bigint NOT NULL,
    state       text NOT NULL,
    reason      text,
    decided_at  timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ob_event_outcome_amount CHECK (amount >= 0),
    CONSTRAINT ob_event_outcome_state CHECK (state IN ('pending', 'applied', 'refused')),
    CONSTRAINT ob_event_outcome_reason CHECK (
        (state = 'refused') = (reason IS NOT NULL)
        AND (reason IS NULL OR reason IN ('limit', 'no_price', 'unknown_account')))
);

-- A member's spend in a month, and an owner's usage per day: read by `ob_allowed` and `ob_daily_usage`.
CREATE INDEX ob_event_outcome_member ON ob_event_outcome (owner, currency, member, occurred_at)
    WHERE state = 'applied';
CREATE INDEX ob_event_outcome_day ON ob_event_outcome (owner, currency, occurred_on) WHERE state = 'applied';

ALTER TABLE ob_txn ADD CONSTRAINT ob_txn_event_fk FOREIGN KEY (event_id) REFERENCES ob_event_outcome (event_id);

-- An outcome moves once, from pending to final, and is never deleted. Trigger functions: `plpgsql`.
CREATE FUNCTION ob_event_outcome_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE'
       OR OLD.state <> 'pending'
       OR NEW.state = 'pending'
       OR (NEW.event_id, NEW.occurred_on, NEW.occurred_at, NEW.owner, NEW.currency, NEW.member, NEW.amount)
          IS DISTINCT FROM
          (OLD.event_id, OLD.occurred_on, OLD.occurred_at, OLD.owner, OLD.currency, OLD.member, OLD.amount) THEN
        RAISE EXCEPTION 'ob_event_outcome: only a pending outcome is decided; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER ob_event_outcome_guard BEFORE UPDATE OR DELETE ON ob_event_outcome
    FOR EACH ROW EXECUTE FUNCTION ob_event_outcome_guard();
CREATE TRIGGER ob_event_outcome_no_truncate BEFORE TRUNCATE ON ob_event_outcome
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();

CREATE FUNCTION ob_check_outcome_decided() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT o.state FROM ob_event_outcome o WHERE o.event_id = NEW.event_id) = 'pending' THEN
        RAISE EXCEPTION 'the outcome of an event was left pending' USING ERRCODE = 'OB902';
    END IF;
    RETURN NULL;
END
$$;

CREATE CONSTRAINT TRIGGER ob_event_outcome_decided AFTER INSERT ON ob_event_outcome
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION ob_check_outcome_decided();

-- ---------------------------------------------------------------------------------------------------------------
-- Partitions (point 10). `plpgsql`: it loops and builds DDL with `format`, which a SQL-standard body cannot run.
-- Creating partitions ahead is here; detaching and dropping old ones is not built yet.

CREATE FUNCTION ob_prepare_partitions(p_window_days integer, p_ahead_days integer) RETURNS integer
    LANGUAGE plpgsql AS $$
DECLARE
    v_today date := ob_utc_day(now());
    v_day   date := ob_utc_day(now()) - (p_window_days - 1);
    v_made  integer := 0;
    v_name  text;
BEGIN
    IF p_window_days < 1 OR p_ahead_days < 0 THEN
        RAISE EXCEPTION 'the window needs a day and the days ahead cannot be negative' USING ERRCODE = '22023';
    END IF;
    WHILE v_day <= v_today + p_ahead_days LOOP
        v_name := 'ob_usage_event_' || to_char(v_day, 'YYYYMMDD');
        IF to_regclass(v_name) IS NULL THEN
            -- Dates as fixed text, never through DateStyle.
            EXECUTE format('CREATE TABLE IF NOT EXISTS %I PARTITION OF ob_usage_event FOR VALUES FROM (%L) TO (%L)',
                           v_name, to_char(v_day, 'YYYY-MM-DD'), to_char(v_day + 1, 'YYYY-MM-DD'));
            v_made := v_made + 1;
        END IF;
        v_day := v_day + 1;
    END LOOP;
    RETURN v_made;
END
$$;
