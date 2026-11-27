-- Rated events (ADR 0004): a raw event carries a meter, a quantity and its dimensions, and keeps the price book
-- version that rated it, on its row and on its outcome. A priced event (ADR 0001, amended 2026-11-27, B4) carries
-- none of them. The price book itself is the next migration.

-- A name of the price book: a meter, a dimension key or a unit. openbilling-core's `MeterId` and `DimensionKey`.
CREATE FUNCTION ob_is_name(p_value text) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN ob_in_alphabet(p_value, 64, 'abcdefghijklmnopqrstuvwxyz0123456789._-');

-- Whether `p_dimensions` is an object of names to ids, at most 16 of them: the dimensions of a raw event or a price.
CREATE FUNCTION ob_is_dimensions(p_dimensions jsonb) RETURNS boolean
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN jsonb_typeof(p_dimensions) = 'object'
        AND (SELECT count(*) FROM jsonb_each(p_dimensions) d) <= 16
        AND NOT EXISTS (SELECT 1 FROM jsonb_each(p_dimensions) d
                        WHERE jsonb_typeof(d.value) <> 'string'
                           OR NOT ob_is_name(d.key)
                           OR NOT ob_is_id(d.value #>> '{}', 128));

-- Both columns are added nullable, so the rows already there (priced events) stay as they are. No foreign key to
-- the version: every insert would take a key-share lock on the one version row in force, which many intakes at once
-- turn into multixacts. The version comes from the rating join in the same statement, and a version is never
-- deleted (the next migration's triggers).
ALTER TABLE ob_usage_event
    ADD COLUMN meter         text,
    ADD COLUMN quantity      bigint,
    ADD COLUMN dimensions    jsonb,
    ADD COLUMN price_version uuid;

-- A raw event has all four, a priced one none.
ALTER TABLE ob_usage_event
    ADD CONSTRAINT ob_usage_event_rated CHECK (
        (meter IS NULL) = (quantity IS NULL)
        AND (meter IS NULL) = (dimensions IS NULL)
        AND (meter IS NULL) = (price_version IS NULL)
        AND (meter IS NULL OR ob_is_name(meter))
        AND (quantity IS NULL OR quantity >= 0)
        AND (dimensions IS NULL OR ob_is_dimensions(dimensions)));

-- The rated events that occurred after they were received: the only ones a new version could take effect before
-- (ADR 0004, *No back-dating*). Small: a producer's clock ahead of the database's, within the skew allowance.
CREATE INDEX ob_usage_event_rated_ahead ON ob_usage_event (occurred_at)
    WHERE occurred_at > received_at AND price_version IS NOT NULL;

-- The outcome keeps what rated the event, so a statement can be derived again from the outcomes and the versions
-- they name after the event's partition is gone (ADR 0001, point 18).
ALTER TABLE ob_event_outcome
    ADD COLUMN meter         text,
    ADD COLUMN quantity      bigint,
    ADD COLUMN dimensions    jsonb,
    ADD COLUMN price_version uuid;

ALTER TABLE ob_event_outcome
    ADD CONSTRAINT ob_event_outcome_rated CHECK (
        (meter IS NULL) = (quantity IS NULL)
        AND (meter IS NULL) = (dimensions IS NULL)
        AND (meter IS NULL) = (price_version IS NULL));

-- The guards, again, with the new columns among those that never change. Trigger functions: `plpgsql`.
CREATE OR REPLACE FUNCTION ob_usage_event_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE'
       OR OLD.state <> 'received'
       OR NEW.state = 'received'
       OR (NEW.event_id, NEW.occurred_on, NEW.occurred_at, NEW.owner, NEW.currency, NEW.member, NEW.amount,
           NEW.payload_hash, NEW.received_at, NEW.meter, NEW.quantity, NEW.dimensions, NEW.price_version)
          IS DISTINCT FROM
          (OLD.event_id, OLD.occurred_on, OLD.occurred_at, OLD.owner, OLD.currency, OLD.member, OLD.amount,
           OLD.payload_hash, OLD.received_at, OLD.meter, OLD.quantity, OLD.dimensions, OLD.price_version) THEN
        RAISE EXCEPTION 'ob_usage_event: only a received event moves to its final state; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

CREATE OR REPLACE FUNCTION ob_event_outcome_guard() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP <> 'UPDATE'
       OR OLD.state <> 'pending'
       OR NEW.state = 'pending'
       OR (NEW.event_id, NEW.occurred_on, NEW.occurred_at, NEW.owner, NEW.currency, NEW.member, NEW.amount,
           NEW.meter, NEW.quantity, NEW.dimensions, NEW.price_version)
          IS DISTINCT FROM
          (OLD.event_id, OLD.occurred_on, OLD.occurred_at, OLD.owner, OLD.currency, OLD.member, OLD.amount,
           OLD.meter, OLD.quantity, OLD.dimensions, OLD.price_version) THEN
        RAISE EXCEPTION 'ob_event_outcome: only a pending outcome is decided; % refused', TG_OP
            USING ERRCODE = 'OB901';
    END IF;
    RETURN NEW;
END
$$;

-- The payload hash of a raw event (ADR 0001, point 3, amended by ADR 0004): SHA-256 over a JSON object with sorted
-- keys and no insignificant whitespace: the currency, the dimensions (an object, keys sorted byte by byte), the
-- member, the meter, the occurrence time in UTC to the microsecond, the owner and the quantity. Not the amount: the
-- service computes it. A priced event keeps `ob_payload_hash`, so the same id and day sent once priced and once raw
-- is a conflict. Plain SQL.
CREATE FUNCTION ob_raw_payload_canonical(p_owner text, p_currency text, p_member text, p_meter text,
                                         p_quantity bigint, p_dimensions jsonb, p_occurred_at timestamptz)
    RETURNS text
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN '{"currency":' || to_json(p_currency)::text
        || ',"dimensions":{'
        || coalesce((SELECT string_agg(to_json(d.key)::text || ':' || to_json(d.value)::text, ','
                                       ORDER BY d.key COLLATE "C")
                     FROM jsonb_each_text(p_dimensions) d), '')
        || '},"member":' || to_json(p_member)::text
        || ',"meter":' || to_json(p_meter)::text
        || ',"occurred_at":"' || to_char(p_occurred_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"') || '"'
        || ',"owner":' || to_json(p_owner)::text
        || ',"quantity":' || p_quantity::text
        || '}';

CREATE FUNCTION ob_raw_payload_hash(p_owner text, p_currency text, p_member text, p_meter text,
                                    p_quantity bigint, p_dimensions jsonb, p_occurred_at timestamptz) RETURNS bytea
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN sha256(convert_to(ob_raw_payload_canonical(p_owner, p_currency, p_member, p_meter, p_quantity,
                                                      p_dimensions, p_occurred_at), 'UTF8'));
