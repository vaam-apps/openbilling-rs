-- The price book (ADR 0004): versions, meters, their dimensions and prices; the loader, the rating, and intake and
-- the drain with raw events. Every rule is here (agent guide, rule 12); the Rust store parses the YAML and calls.
--
-- SQLSTATEs raised here (mapped in `openbilling-pg/src/sqlstate.rs`):
--   OB109 negative_quantity    OB110 unpriced_meter    OB111 unpriced_dimension   (events named in DETAIL)
--   OB201 invalid_price_list   OB202 price_list_duplicate   OB203 price_list_mismatch
--   OB204 unknown_currency (in a price list)   OB205 back_dated   OB206 version_conflict
--   (a price list's position in DETAIL: `<line> <column> <key>`, separated by one space; a key holds no space)
--   OB903 a version whose content is not its hash: a fault.
-- OB101 to OB108 keep their meaning; OB108 (overflow) also refuses a raw event rated past the largest amount.

-- ---------------------------------------------------------------------------------------------------------------
-- Helpers. Plain SQL.

-- `p_numerator ÷ p_denominator`, rounded to an integer, half to even (ADR 0004, *Rounding*): the one rounding of a
-- rated event. For a numerator of zero or more and a denominator of one or more, which is all rating gives it.
CREATE FUNCTION ob_round_half_even(p_numerator numeric, p_denominator numeric) RETURNS numeric
    LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
    RETURN div(p_numerator, p_denominator)
        + CASE
              WHEN 2 * mod(p_numerator, p_denominator) > p_denominator THEN 1
              WHEN 2 * mod(p_numerator, p_denominator) < p_denominator THEN 0
              ELSE mod(div(p_numerator, p_denominator), 2)
          END;

-- An object of dimensions from parallel arrays of keys and values. A key given twice raises 22030 (only a malformed
-- call does that: openbilling-core's `Dimensions` holds each key once).
CREATE FUNCTION ob_dimensions(p_keys text[], p_values text[]) RETURNS jsonb
    LANGUAGE sql IMMUTABLE PARALLEL SAFE
    RETURN coalesce((SELECT jsonb_object_agg_unique(d.key, d.value)
                     FROM unnest(p_keys, p_values) AS d(key, value)), '{}'::jsonb);

-- ---------------------------------------------------------------------------------------------------------------
-- Tables. A version is written once, by `ob_load_price_list`, with its meters, dimensions and prices, and never
-- changes (ADR 0001, point 18). Its children are written first and the version row last: the version row seals
-- them (its trigger checks that its content hash is theirs), and no child can be added to a sealed version.

CREATE TABLE ob_price_version (
    version_id     uuid PRIMARY KEY DEFAULT uuidv7(),
    effective_from timestamptz NOT NULL,
    content_hash   bytea NOT NULL,
    loaded_at      timestamptz NOT NULL DEFAULT now(),
    -- The version in force at an instant is the one with the greatest `effective_from` not after it.
    CONSTRAINT ob_price_version_effective_from_key UNIQUE (effective_from),
    -- Loading the same content twice is a no-op: the hash is the once-only key.
    CONSTRAINT ob_price_version_content_hash_key UNIQUE (content_hash),
    CONSTRAINT ob_price_version_hash CHECK (octet_length(content_hash) = 32)
);

CREATE TABLE ob_price_meter (
    version_id uuid NOT NULL REFERENCES ob_price_version (version_id) DEFERRABLE INITIALLY DEFERRED,
    meter      text NOT NULL,
    unit       text,
    PRIMARY KEY (version_id, meter),
    CONSTRAINT ob_price_meter_names CHECK (ob_is_name(meter) AND (unit IS NULL OR ob_is_name(unit)))
);

-- The dimension keys a meter is priced by, in this version.
CREATE TABLE ob_price_dimension (
    version_id uuid NOT NULL,
    meter      text NOT NULL,
    dimension  text NOT NULL,
    PRIMARY KEY (version_id, meter, dimension),
    FOREIGN KEY (version_id, meter) REFERENCES ob_price_meter (version_id, meter),
    CONSTRAINT ob_price_dimension_name CHECK (ob_is_name(dimension))
);

-- A price: `price_micros` micro-units of the currency per `per_units` of the meter's unit, for exactly these
-- dimensions. A rational, so a price below one micro-unit per unit (a token's) is exact.
CREATE TABLE ob_price (
    version_id   uuid NOT NULL,
    meter        text NOT NULL,
    currency     text NOT NULL REFERENCES ob_currency (code),
    dimensions   jsonb NOT NULL,
    price_micros bigint NOT NULL,
    per_units    bigint NOT NULL,
    PRIMARY KEY (version_id, meter, currency, dimensions),
    FOREIGN KEY (version_id, meter) REFERENCES ob_price_meter (version_id, meter),
    CONSTRAINT ob_price_dimensions CHECK (ob_is_dimensions(dimensions)),
    CONSTRAINT ob_price_amounts CHECK (price_micros >= 0 AND per_units >= 1)
);

-- ---------------------------------------------------------------------------------------------------------------
-- The content hash: SHA-256 over a canonical document of a version. Plain SQL.
--
-- The document is a jsonb object `{"effective_from", "meters", "prices"}`: the time in UTC to the microsecond,
-- the meters sorted by name (each with its unit and its dimension keys sorted), the prices sorted by meter, currency
-- and dimensions. Equal jsonb values print as equal text, so two lists that differ only in layout, comments, order or
-- the spelling of the time have one hash; a list that changes a price or the time has another.
CREATE FUNCTION ob_price_content_hash(p_effective_from timestamptz, p_meters jsonb, p_prices jsonb) RETURNS bytea
    LANGUAGE sql STABLE STRICT PARALLEL SAFE
    RETURN sha256(convert_to('openbilling price list v1' || chr(10) || jsonb_build_object(
        'effective_from', to_char(p_effective_from AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"'),
        'meters', coalesce((
            SELECT jsonb_agg(jsonb_build_object(
                       'meter', m.value ->> 'meter',
                       'unit', m.value -> 'unit',
                       'dimensions', coalesce((SELECT jsonb_agg(d.value ORDER BY d.value COLLATE "C")
                                               FROM jsonb_array_elements_text(m.value -> 'dimensions') d),
                                              '[]'::jsonb))
                   ORDER BY m.value ->> 'meter' COLLATE "C")
            FROM jsonb_array_elements(p_meters) m), '[]'::jsonb),
        'prices', coalesce((
            SELECT jsonb_agg(jsonb_build_object(
                       'meter', p.value ->> 'meter',
                       'currency', p.value ->> 'currency',
                       'dimensions', p.value -> 'dimensions',
                       'price_micros', (p.value ->> 'price_micros')::bigint,
                       'per_units', (p.value ->> 'per_units')::bigint)
                   ORDER BY p.value ->> 'meter' COLLATE "C", p.value ->> 'currency' COLLATE "C",
                            p.value -> 'dimensions')
            FROM jsonb_array_elements(p_prices) p), '[]'::jsonb))::text, 'UTF8'));

-- A stored version's content hash, from its rows.
CREATE FUNCTION ob_price_version_hash(p_version uuid, p_effective_from timestamptz) RETURNS bytea
    LANGUAGE sql STABLE STRICT
    RETURN ob_price_content_hash(
        p_effective_from,
        coalesce((SELECT jsonb_agg(jsonb_build_object(
                             'meter', m.meter,
                             'unit', m.unit,
                             'dimensions', coalesce((SELECT jsonb_agg(d.dimension)
                                                     FROM ob_price_dimension d
                                                     WHERE d.version_id = m.version_id AND d.meter = m.meter),
                                                    '[]'::jsonb)))
                  FROM ob_price_meter m WHERE m.version_id = p_version), '[]'::jsonb),
        coalesce((SELECT jsonb_agg(jsonb_build_object(
                             'meter', p.meter, 'currency', p.currency, 'dimensions', p.dimensions,
                             'price_micros', p.price_micros, 'per_units', p.per_units))
                  FROM ob_price p WHERE p.version_id = p_version), '[]'::jsonb));

-- ---------------------------------------------------------------------------------------------------------------
-- No back-dating (ADR 0001, point 19; ADR 0004). A version may take effect only after now, and only after every
-- event already rated: otherwise an event rated by the version before it would fall where it is in force.
--
-- Every rated event stored before the version's lock was received before the clock this reads, so one that occurred
-- at or after a time later than that clock occurred after it was received, and is in `ob_usage_event_rated_ahead`.
-- Called under the lock below. `clock_timestamp()`, not `now()`: the lock may have been waited for. Plain SQL.
CREATE FUNCTION ob_price_back_dated(p_effective_from timestamptz) RETURNS boolean
    LANGUAGE sql VOLATILE STRICT
    RETURN p_effective_from <= clock_timestamp()
        OR EXISTS (SELECT 1 FROM ob_usage_event e
                   WHERE e.occurred_at > e.received_at AND e.price_version IS NOT NULL
                     AND e.occurred_at >= p_effective_from);

-- ---------------------------------------------------------------------------------------------------------------
-- Guards. Trigger functions are `plpgsql`.
--
-- The lock: writing a version takes `ob_price_version` in EXCLUSIVE mode, and rating at intake takes it in ROW SHARE
-- mode, which conflict, while a plain read (ACCESS SHARE) conflicts with neither. So a version is written while no
-- intake is rating, and an intake that rates after it sees it.

-- Nothing of a version changes: a correction is a new version, or a credit (ADR 0001, point 19).
CREATE TRIGGER ob_price_version_append_only BEFORE UPDATE OR DELETE ON ob_price_version
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_version_no_truncate BEFORE TRUNCATE ON ob_price_version
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_meter_append_only BEFORE UPDATE OR DELETE ON ob_price_meter
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_meter_no_truncate BEFORE TRUNCATE ON ob_price_meter
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_dimension_append_only BEFORE UPDATE OR DELETE ON ob_price_dimension
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_dimension_no_truncate BEFORE TRUNCATE ON ob_price_dimension
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_append_only BEFORE UPDATE OR DELETE ON ob_price
    FOR EACH ROW EXECUTE FUNCTION ob_refuse_change();
CREATE TRIGGER ob_price_no_truncate BEFORE TRUNCATE ON ob_price
    FOR EACH STATEMENT EXECUTE FUNCTION ob_refuse_change();

-- A version row is written last, under the lock, and seals its children: it is refused when back-dated (OB205) or
-- when its hash is not the hash of its rows (OB903).
CREATE FUNCTION ob_price_version_seal() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    LOCK TABLE ob_price_version IN EXCLUSIVE MODE;
    IF ob_price_back_dated(NEW.effective_from) THEN
        RAISE EXCEPTION 'the version would take effect before now or before an event already rated'
            USING ERRCODE = 'OB205';
    END IF;
    IF NEW.content_hash IS DISTINCT FROM ob_price_version_hash(NEW.version_id, NEW.effective_from) THEN
        RAISE EXCEPTION 'the version''s hash is not the hash of its rows' USING ERRCODE = 'OB903';
    END IF;
    RETURN NEW;
END
$$;

CREATE TRIGGER ob_price_version_seal BEFORE INSERT ON ob_price_version
    FOR EACH ROW EXECUTE FUNCTION ob_price_version_seal();

-- A meter, a dimension or a price is written only for a version not written yet: once a version row exists, nothing
-- is added to it.
CREATE FUNCTION ob_price_child_of_unsealed() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    LOCK TABLE ob_price_version IN EXCLUSIVE MODE;
    IF EXISTS (SELECT 1 FROM added a JOIN ob_price_version v ON v.version_id = a.version_id) THEN
        RAISE EXCEPTION '% refused: the price book version is sealed', TG_TABLE_NAME USING ERRCODE = 'OB901';
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER ob_price_meter_unsealed AFTER INSERT ON ob_price_meter
    REFERENCING NEW TABLE AS added FOR EACH STATEMENT EXECUTE FUNCTION ob_price_child_of_unsealed();
CREATE TRIGGER ob_price_dimension_unsealed AFTER INSERT ON ob_price_dimension
    REFERENCING NEW TABLE AS added FOR EACH STATEMENT EXECUTE FUNCTION ob_price_child_of_unsealed();
CREATE TRIGGER ob_price_unsealed AFTER INSERT ON ob_price
    REFERENCING NEW TABLE AS added FOR EACH STATEMENT EXECUTE FUNCTION ob_price_child_of_unsealed();

-- ---------------------------------------------------------------------------------------------------------------
-- The loader. `plpgsql`: it checks the list in order and raises a SQLSTATE naming the position of the first value
-- that breaks a rule, and branches on a replay.
--
-- The Rust store parses the YAML and passes its values as arrays, with the line and column each came from:
--   p_top: the line and column of `effective_from`, `meters` and `prices` (six integers);
--   the meters, and their dimension keys by meter (1-based);
--   the prices, and their dimensions by price (1-based). A price's `per` is NULL when the list leaves it out: 1.
-- The checks, in order: the time's form; one to 1000 meters; each meter's name, unit and at most 16 dimension keys,
-- each a name, none twice; no meter twice; one to 100000 prices; each price's meter declared, its currency
-- configured, its price at least zero and its `per` at least one, its dimensions names and ids and exactly its
-- meter's keys; no price twice. Then, under the lock: the same content already loaded is a replay; a back-dated
-- version is refused; a time another version takes effect at is a conflict. Then the rows, the version last.
CREATE FUNCTION ob_load_price_list(
    p_effective_from text, p_top integer[],
    p_meter_names text[], p_meter_units text[], p_meter_lines integer[], p_meter_columns integer[],
    p_meter_dimension_of integer[], p_meter_dimensions text[],
    p_price_meters text[], p_price_currencies text[], p_price_micros bigint[], p_price_per bigint[],
    p_price_lines integer[], p_price_columns integer[],
    p_price_dimension_of integer[], p_price_dimension_keys text[], p_price_dimension_values text[])
    RETURNS TABLE (o_version_id uuid, o_effective_micros bigint, o_content_hash text, o_replayed boolean)
    LANGUAGE plpgsql AS $$
DECLARE
    v_effective timestamptz;
    v_shape     text := translate(p_effective_from, '0123456789', '9999999999');
    v_code      text;
    v_at        text;
    v_meters    jsonb;
    v_prices    jsonb;
    v_hash      bytea;
    v_version   uuid;
    v_stored    timestamptz;
BEGIN
    IF cardinality(p_top) IS DISTINCT FROM 6
       OR cardinality(p_meter_units) IS DISTINCT FROM cardinality(p_meter_names)
       OR cardinality(p_meter_lines) IS DISTINCT FROM cardinality(p_meter_names)
       OR cardinality(p_meter_columns) IS DISTINCT FROM cardinality(p_meter_names)
       OR cardinality(p_meter_dimensions) IS DISTINCT FROM cardinality(p_meter_dimension_of)
       OR cardinality(p_price_currencies) IS DISTINCT FROM cardinality(p_price_meters)
       OR cardinality(p_price_micros) IS DISTINCT FROM cardinality(p_price_meters)
       OR cardinality(p_price_per) IS DISTINCT FROM cardinality(p_price_meters)
       OR cardinality(p_price_lines) IS DISTINCT FROM cardinality(p_price_meters)
       OR cardinality(p_price_columns) IS DISTINCT FROM cardinality(p_price_meters)
       OR cardinality(p_price_dimension_keys) IS DISTINCT FROM cardinality(p_price_dimension_of)
       OR cardinality(p_price_dimension_values) IS DISTINCT FROM cardinality(p_price_dimension_of)
       OR EXISTS (SELECT 1 FROM unnest(p_meter_dimension_of) i WHERE i IS NULL OR i < 1
                                                                 OR i > cardinality(p_meter_names))
       OR EXISTS (SELECT 1 FROM unnest(p_price_dimension_of) i WHERE i IS NULL OR i < 1
                                                                 OR i > cardinality(p_price_meters)) THEN
        RAISE EXCEPTION 'malformed price list call' USING ERRCODE = '22023';
    END IF;

    -- The time: `YYYY-MM-DDTHH:MM:SS`, an optional fraction of one to six digits, and `Z`. Checked by its shape,
    -- digit for digit, so neither a locale nor the session's time zone nor PostgreSQL's words (`now`, `epoch`) count.
    v_at := p_top[1] || ' ' || p_top[2] || ' effective_from';
    IF p_effective_from IS NULL
       OR NOT (v_shape = '9999-99-99T99:99:99Z' OR v_shape ~ '^9999-99-99T99:99:99\.9{1,6}Z$') THEN
        RAISE EXCEPTION 'the price list is refused: a value outside its form' USING ERRCODE = 'OB201', DETAIL = v_at;
    END IF;
    BEGIN
        v_effective := replace(p_effective_from, 'Z', '+00:00')::timestamptz;
    EXCEPTION WHEN datetime_field_overflow OR invalid_datetime_format THEN
        RAISE EXCEPTION 'the price list is refused: a value outside its form' USING ERRCODE = 'OB201', DETAIL = v_at;
    END;

    -- The meters.
    IF coalesce(cardinality(p_meter_names), 0) NOT BETWEEN 1 AND 1000 THEN
        RAISE EXCEPTION 'the price list is refused: a value outside its form'
            USING ERRCODE = 'OB201', DETAIL = p_top[3] || ' ' || p_top[4] || ' meters';
    END IF;
    SELECT x.at INTO v_at FROM (
        SELECT m.line || ' ' || m.col || ' meters[' || (m.i - 1) || '].name' AS at, m.i, 1 AS field
        FROM unnest(p_meter_names, p_meter_lines, p_meter_columns) WITH ORDINALITY AS m(name, line, col, i)
        WHERE m.name IS NULL OR NOT ob_is_name(m.name)
        UNION ALL
        SELECT m.line || ' ' || m.col || ' meters[' || (m.i - 1) || '].unit', m.i, 2
        FROM unnest(p_meter_units, p_meter_lines, p_meter_columns) WITH ORDINALITY AS m(unit, line, col, i)
        WHERE m.unit IS NOT NULL AND NOT ob_is_name(m.unit)
        UNION ALL
        SELECT p_meter_lines[d.of] || ' ' || p_meter_columns[d.of] || ' meters[' || (d.of - 1) || '].dimensions',
               d.of, 3
        FROM unnest(p_meter_dimension_of) AS d(of)
        GROUP BY d.of HAVING count(*) > 16
        UNION ALL
        SELECT p_meter_lines[d.of] || ' ' || p_meter_columns[d.of] || ' meters[' || (d.of - 1) || '].dimensions['
                   || (d.position - 1) || ']', d.of, 4
        FROM (SELECT e.of, e.name, row_number() OVER (PARTITION BY e.of ORDER BY e.n) AS position
              FROM unnest(p_meter_dimension_of, p_meter_dimensions) WITH ORDINALITY AS e(of, name, n)) d
        WHERE d.name IS NULL OR NOT ob_is_name(d.name)
    ) x ORDER BY x.i, x.field LIMIT 1;
    IF v_at IS NOT NULL THEN
        RAISE EXCEPTION 'the price list is refused: a value outside its form' USING ERRCODE = 'OB201', DETAIL = v_at;
    END IF;

    -- A meter or one of its dimension keys given twice: the later copy is named.
    SELECT x.at INTO v_at FROM (
        SELECT m.line || ' ' || m.col || ' meters[' || (m.i - 1) || '].name' AS at, m.i, 1 AS field
        FROM (SELECT n.line, n.col, n.i, row_number() OVER (PARTITION BY n.name ORDER BY n.i) AS copy
              FROM unnest(p_meter_names, p_meter_lines, p_meter_columns) WITH ORDINALITY AS n(name, line, col, i)) m
        WHERE m.copy > 1
        UNION ALL
        SELECT p_meter_lines[d.of] || ' ' || p_meter_columns[d.of] || ' meters[' || (d.of - 1) || '].dimensions['
                   || (d.position - 1) || ']', d.of, 2
        FROM (SELECT e.of, row_number() OVER (PARTITION BY e.of ORDER BY e.n) AS position,
                     row_number() OVER (PARTITION BY e.of, e.name ORDER BY e.n) AS copy
              FROM unnest(p_meter_dimension_of, p_meter_dimensions) WITH ORDINALITY AS e(of, name, n)) d
        WHERE d.copy > 1
    ) x ORDER BY x.i, x.field LIMIT 1;
    IF v_at IS NOT NULL THEN
        RAISE EXCEPTION 'the price list is refused: a meter, a dimension or a price given twice'
            USING ERRCODE = 'OB202', DETAIL = v_at;
    END IF;

    -- The prices: one to 100000.
    IF coalesce(cardinality(p_price_meters), 0) NOT BETWEEN 1 AND 100000 THEN
        RAISE EXCEPTION 'the price list is refused: a value outside its form'
            USING ERRCODE = 'OB201', DETAIL = p_top[5] || ' ' || p_top[6] || ' prices';
    END IF;

    -- Each price, in order, field by field; the first price that breaks a rule is named, with its first bad field.
    WITH price AS (
        SELECT p.*
        FROM unnest(p_price_meters, p_price_currencies, p_price_micros, p_price_per, p_price_lines, p_price_columns)
             WITH ORDINALITY AS p(meter, currency, micros, per, line, col, i)
    ), given AS (
        SELECT g.of, jsonb_agg(g.key ORDER BY g.key COLLATE "C") AS keys,
               bool_and(g.key IS NOT NULL AND g.value IS NOT NULL AND ob_is_name(g.key) AND ob_is_id(g.value, 128))
                   AND count(*) <= 16 AND count(DISTINCT g.key) = count(*) AS well_formed
        FROM unnest(p_price_dimension_of, p_price_dimension_keys, p_price_dimension_values) AS g(of, key, value)
        GROUP BY g.of
    ), declared AS (
        SELECT m.name, m.i,
               coalesce((SELECT jsonb_agg(d.name ORDER BY d.name COLLATE "C")
                         FROM unnest(p_meter_dimension_of, p_meter_dimensions) AS d(of, name)
                         WHERE d.of = m.i), '[]'::jsonb) AS keys
        FROM unnest(p_meter_names) WITH ORDINALITY AS m(name, i)
    ), problem AS (
        SELECT p.i, f.field, f.code, f.key
        FROM price p
        LEFT JOIN declared m ON m.name = p.meter
        LEFT JOIN given g ON g.of = p.i
        CROSS JOIN LATERAL (VALUES
            (1, 'OB203', 'meter', m.name IS NULL),
            (2, 'OB204', 'currency', p.currency IS NULL
                                     OR NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = p.currency)),
            (3, 'OB201', 'price_micros', p.micros IS NULL OR p.micros < 0),
            (4, 'OB201', 'per', p.per IS NOT NULL AND p.per < 1),
            (5, 'OB201', 'dimensions', g.of IS NOT NULL AND NOT g.well_formed),
            (6, 'OB203', 'dimensions', m.name IS NOT NULL AND coalesce(g.keys, '[]'::jsonb) <> m.keys)
        ) AS f(field, code, key, broken)
        WHERE f.broken
    )
    SELECT x.code, p.line || ' ' || p.col || ' prices[' || (p.i - 1) || '].' || x.key
    INTO v_code, v_at
    FROM problem x JOIN price p ON p.i = x.i
    ORDER BY x.i, x.field
    LIMIT 1;
    IF v_at IS NOT NULL THEN
        CASE v_code
            WHEN 'OB203' THEN
                RAISE EXCEPTION 'the price list is refused: a price that does not fit its meter'
                    USING ERRCODE = 'OB203', DETAIL = v_at;
            WHEN 'OB204' THEN
                RAISE EXCEPTION 'the price list is refused: a currency the operator has not named'
                    USING ERRCODE = 'OB204', DETAIL = v_at;
            ELSE
                RAISE EXCEPTION 'the price list is refused: a value outside its form'
                    USING ERRCODE = 'OB201', DETAIL = v_at;
        END CASE;
    END IF;

    -- The document, as the hash reads it.
    SELECT coalesce(jsonb_agg(jsonb_build_object(
               'meter', m.name, 'unit', m.unit,
               'dimensions', coalesce((SELECT jsonb_agg(d.name)
                                       FROM unnest(p_meter_dimension_of, p_meter_dimensions) AS d(of, name)
                                       WHERE d.of = m.i), '[]'::jsonb))), '[]'::jsonb)
    INTO v_meters
    FROM unnest(p_meter_names, p_meter_units) WITH ORDINALITY AS m(name, unit, i);
    SELECT jsonb_agg(jsonb_build_object(
               'meter', p.meter, 'currency', p.currency,
               'dimensions', coalesce((SELECT jsonb_object_agg(g.key, g.value)
                                       FROM unnest(p_price_dimension_of, p_price_dimension_keys,
                                                   p_price_dimension_values) AS g(of, key, value)
                                       WHERE g.of = p.i), '{}'::jsonb),
               'price_micros', p.micros, 'per_units', coalesce(p.per, 1)))
    INTO v_prices
    FROM unnest(p_price_meters, p_price_currencies, p_price_micros, p_price_per) WITH ORDINALITY
         AS p(meter, currency, micros, per, i);

    -- A price given twice: same meter, currency and dimensions. The later copy is named.
    SELECT x.line || ' ' || x.col || ' prices[' || (x.i - 1) || ']' INTO v_at
    FROM (SELECT p.i, p_price_lines[p.i] AS line, p_price_columns[p.i] AS col,
                 row_number() OVER (PARTITION BY p.value ->> 'meter', p.value ->> 'currency', p.value -> 'dimensions'
                                    ORDER BY p.i) AS copy
          FROM jsonb_array_elements(v_prices) WITH ORDINALITY AS p(value, i)) x
    WHERE x.copy > 1
    ORDER BY x.i
    LIMIT 1;
    IF v_at IS NOT NULL THEN
        RAISE EXCEPTION 'the price list is refused: a meter, a dimension or a price given twice'
            USING ERRCODE = 'OB202', DETAIL = v_at;
    END IF;

    v_hash := ob_price_content_hash(v_effective, v_meters, v_prices);
    v_at := p_top[1] || ' ' || p_top[2] || ' effective_from';

    -- Under the lock: no intake rates while a version is written, and two loaders take turns.
    LOCK TABLE ob_price_version IN EXCLUSIVE MODE;

    SELECT v.version_id, v.effective_from INTO v_version, v_stored
    FROM ob_price_version v WHERE v.content_hash = v_hash;
    IF FOUND THEN
        RETURN QUERY SELECT v_version, ob_micros(v_stored), encode(v_hash, 'hex'), true;
        RETURN;
    END IF;
    IF ob_price_back_dated(v_effective) THEN
        RAISE EXCEPTION 'the version would take effect before now or before an event already rated'
            USING ERRCODE = 'OB205', DETAIL = v_at;
    END IF;
    IF EXISTS (SELECT 1 FROM ob_price_version v WHERE v.effective_from = v_effective) THEN
        RAISE EXCEPTION 'another version takes effect at the same time' USING ERRCODE = 'OB206', DETAIL = v_at;
    END IF;

    v_version := uuidv7();
    INSERT INTO ob_price_meter (version_id, meter, unit)
        SELECT v_version, m.value ->> 'meter', m.value ->> 'unit' FROM jsonb_array_elements(v_meters) m;
    INSERT INTO ob_price_dimension (version_id, meter, dimension)
        SELECT v_version, m.value ->> 'meter', d.value
        FROM jsonb_array_elements(v_meters) m, jsonb_array_elements_text(m.value -> 'dimensions') d;
    INSERT INTO ob_price (version_id, meter, currency, dimensions, price_micros, per_units)
        SELECT v_version, p.value ->> 'meter', p.value ->> 'currency', p.value -> 'dimensions',
               (p.value ->> 'price_micros')::bigint, (p.value ->> 'per_units')::bigint
        FROM jsonb_array_elements(v_prices) p;
    INSERT INTO ob_price_version (version_id, effective_from, content_hash) VALUES (v_version, v_effective, v_hash);

    RETURN QUERY SELECT v_version, ob_micros(v_effective), encode(v_hash, 'hex'), false;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Rating (ADR 0004).

-- The price of a meter in a currency for exactly these dimensions, in the version in force at `p_at`: the one with
-- the greatest `effective_from` not after it. One row: the version (NULL when none is in force), the price and its
-- `per` (NULL when unpriced), and the problem: NULL when priced; `meter` when the meter has no price in the currency
-- in that version, or no version is in force; `dimension` when it has, but not for these dimensions. Plain SQL.
CREATE FUNCTION ob_price_of(p_currency text, p_meter text, p_dimensions jsonb, p_at timestamptz)
    RETURNS TABLE (o_version uuid, o_price_micros bigint, o_per_units bigint, o_problem text)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    WITH in_force AS (
        SELECT v.version_id FROM ob_price_version v
        WHERE v.effective_from <= p_at
        ORDER BY v.effective_from DESC
        LIMIT 1
    )
    SELECT f.version_id, p.price_micros, p.per_units,
           CASE
               WHEN p.version_id IS NOT NULL THEN NULL
               WHEN EXISTS (SELECT 1 FROM ob_price q
                            WHERE q.version_id = f.version_id AND q.meter = p_meter AND q.currency = p_currency)
                   THEN 'dimension'
               ELSE 'meter'
           END
    FROM (VALUES (1)) AS one (n)
    LEFT JOIN in_force f ON true
    LEFT JOIN ob_price p ON p.version_id = f.version_id AND p.meter = p_meter AND p.currency = p_currency
                        AND p.dimensions = p_dimensions;
END;

-- What a raw event costs, and the version that prices it; writes nothing. `plpgsql`: it raises OB109, OB107, OB110,
-- OB111 and OB108 for the one event (no event is named in DETAIL). The amount is `quantity × price ÷ per`, in
-- numeric, rounded once to the micro-unit, half to even, and refused past the largest amount.
CREATE FUNCTION ob_rate(p_currency text, p_meter text, p_quantity bigint, p_dimension_keys text[],
                        p_dimension_values text[], p_occurred_micros bigint)
    RETURNS TABLE (o_amount bigint, o_version uuid)
    LANGUAGE plpgsql AS $$
DECLARE
    v_price  record;
    v_amount numeric;
BEGIN
    IF p_quantity < 0 THEN
        RAISE EXCEPTION 'a negative quantity' USING ERRCODE = 'OB109';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = p_currency) THEN
        RAISE EXCEPTION 'a currency the operator has not named' USING ERRCODE = 'OB107';
    END IF;
    SELECT * INTO v_price
    FROM ob_price_of(p_currency, p_meter, ob_dimensions(p_dimension_keys, p_dimension_values),
                     ob_ts(p_occurred_micros));
    IF v_price.o_problem = 'meter' THEN
        RAISE EXCEPTION 'the meter has no price in the currency at that time' USING ERRCODE = 'OB110';
    ELSIF v_price.o_problem = 'dimension' THEN
        RAISE EXCEPTION 'the meter has no price for these dimensions' USING ERRCODE = 'OB111';
    END IF;
    v_amount := ob_round_half_even(p_quantity::numeric * v_price.o_price_micros, v_price.o_per_units);
    IF v_amount > 9223372036854775807 THEN
        RAISE EXCEPTION 'an amount past the largest amount' USING ERRCODE = 'OB108';
    END IF;
    RETURN QUERY SELECT v_amount::bigint, v_price.o_version;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Intake, now with raw events (ADR 0001, points 1 to 9; ADR 0004). The previous two-function form is replaced: a row
-- is priced (an amount, no meter) or raw (a meter and a quantity, its dimensions, no amount).

DROP FUNCTION ob_intake(text[], text[], text[], text[], bigint[], bigint[], integer, bigint);
DROP FUNCTION ob_intake_rows(text[], text[], text[], text[], bigint[], bigint[]);

-- The batch as rows, with its position, its occurrence time and its payload hash: `ob_payload_hash` for a priced
-- row, `ob_raw_payload_hash` for a raw one. `p_amounts` and `p_versions` hold a raw row's rating once it is known.
-- Plain SQL.
CREATE FUNCTION ob_intake_rows(p_event_ids text[], p_owners text[], p_currencies text[], p_members text[],
                               p_amounts bigint[], p_occurred_micros bigint[], p_meters text[],
                               p_quantities bigint[], p_dimensions jsonb[], p_versions uuid[])
    RETURNS TABLE (i_position bigint, i_event_id text, i_owner text, i_currency text, i_member text,
                   i_amount bigint, i_occurred_at timestamptz, i_meter text, i_quantity bigint,
                   i_dimensions jsonb, i_version uuid, i_payload_hash bytea)
    LANGUAGE sql STABLE
BEGIN ATOMIC
    SELECT t.pos, t.event_id, t.owner, t.currency, t.member, t.amount, ob_ts(t.occurred_micros), t.meter,
           t.quantity, t.dimensions, t.version,
           CASE WHEN t.meter IS NULL
                THEN ob_payload_hash(t.owner, t.currency, t.member, t.amount, ob_ts(t.occurred_micros))
                ELSE ob_raw_payload_hash(t.owner, t.currency, t.member, t.meter, t.quantity, t.dimensions,
                                         ob_ts(t.occurred_micros))
           END
    FROM unnest(p_event_ids, p_owners, p_currencies, p_members, p_amounts, p_occurred_micros, p_meters,
                p_quantities, p_dimensions, p_versions)
         WITH ORDINALITY AS t(event_id, owner, currency, member, amount, occurred_micros, meter, quantity,
                              dimensions, version, pos);
END;

-- Takes a batch in, all or nothing. `plpgsql`: each rule raises its own SQLSTATE naming the events that broke it,
-- in batch order, and a raise rolls the whole call back. The rules, in order: at most 1000 events; no negative
-- amount; no negative quantity; a repeated id only as an identical copy; a currency the operator named; the window,
-- against the database's clock (point 8); then, for the raw rows, under the price book's ROW SHARE lock, rating by
-- the version in force at each occurrence time: no unpriced meter, no unpriced dimension, no amount past the
-- largest; then, per owner and currency, amounts that add up to one `Amount`; no stored event of the same id and day
-- with another payload (point 2). Then one statement writes the rest: `INSERT … ON CONFLICT … DO SELECT … RETURNING
-- old…` (A1, A2), which takes no row lock (point 4). Returns, in batch order, `accepted` or `replayed` (a later copy
-- of an id in the batch is a replay).
--
-- A raw row's dimensions come as `p_dimension_of` (its 1-based position), `p_dimension_keys` and
-- `p_dimension_values`.
CREATE FUNCTION ob_intake(p_event_ids text[], p_owners text[], p_currencies text[], p_members text[],
                          p_amounts bigint[], p_occurred_micros bigint[], p_meters text[], p_quantities bigint[],
                          p_dimension_of integer[], p_dimension_keys text[], p_dimension_values text[],
                          p_window_days integer, p_skew_micros bigint)
    RETURNS TABLE (o_event_id text, o_answer text)
    LANGUAGE plpgsql AS $$
DECLARE
    v_named      text;
    v_ids        text[];
    v_inserted   boolean[];
    v_conflicts  text;
    v_dimensions jsonb[];
    v_amounts    bigint[] := p_amounts;
    v_versions   uuid[];
    v_rated      record;
BEGIN
    IF coalesce(cardinality(p_event_ids), 0) > 1000 THEN
        RAISE EXCEPTION 'the batch holds more than 1000 events' USING ERRCODE = 'OB101';
    END IF;
    IF p_window_days < 1 OR p_skew_micros < 0
       OR cardinality(p_owners) <> cardinality(p_event_ids) OR cardinality(p_currencies) <> cardinality(p_event_ids)
       OR cardinality(p_members) <> cardinality(p_event_ids) OR cardinality(p_amounts) <> cardinality(p_event_ids)
       OR cardinality(p_occurred_micros) <> cardinality(p_event_ids)
       OR cardinality(p_meters) <> cardinality(p_event_ids) OR cardinality(p_quantities) <> cardinality(p_event_ids)
       OR cardinality(p_dimension_keys) IS DISTINCT FROM cardinality(p_dimension_of)
       OR cardinality(p_dimension_values) IS DISTINCT FROM cardinality(p_dimension_of)
       -- A row is priced or raw, never both nor neither; only a raw row has dimensions.
       OR EXISTS (SELECT 1 FROM unnest(p_amounts, p_meters, p_quantities) AS r(amount, meter, quantity)
                  WHERE (r.amount IS NULL) = (r.meter IS NULL) OR (r.meter IS NULL) <> (r.quantity IS NULL))
       OR EXISTS (SELECT 1 FROM unnest(p_dimension_of) AS d(of)
                  WHERE d.of IS NULL OR d.of < 1 OR d.of > cardinality(p_event_ids) OR p_meters[d.of] IS NULL) THEN
        RAISE EXCEPTION 'malformed intake call' USING ERRCODE = '22023';
    END IF;
    IF coalesce(cardinality(p_event_ids), 0) = 0 THEN
        RETURN;
    END IF;

    -- Each raw row's dimensions as one object (a key given twice raises 22030: a malformed call).
    SELECT array_agg(CASE WHEN t.meter IS NOT NULL THEN coalesce(d.dimensions, '{}'::jsonb) END ORDER BY t.pos),
           array_agg(NULL::uuid ORDER BY t.pos)
    INTO v_dimensions, v_versions
    FROM unnest(p_meters) WITH ORDINALITY AS t(meter, pos)
    LEFT JOIN (SELECT k.of, jsonb_object_agg_unique(k.key, k.value) AS dimensions
               FROM unnest(p_dimension_of, p_dimension_keys, p_dimension_values) AS k(of, key, value)
               GROUP BY k.of) d ON d.of = t.pos;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE r.i_amount < 0 GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a negative amount' USING ERRCODE = 'OB102', DETAIL = v_named;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE r.i_quantity < 0 GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a negative quantity' USING ERRCODE = 'OB109', DETAIL = v_named;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          GROUP BY r.i_event_id HAVING count(DISTINCT r.i_payload_hash) > 1) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a repeat that differs from its first copy'
            USING ERRCODE = 'OB103', DETAIL = v_named;
    END IF;

    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE NOT EXISTS (SELECT 1 FROM ob_currency c WHERE c.code = r.i_currency)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: a currency the operator has not named'
            USING ERRCODE = 'OB107', DETAIL = v_named;
    END IF;

    -- The window: no earlier than the start of the UTC day `p_window_days - 1` days ago.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE ob_utc_day(r.i_occurred_at) < ob_utc_day(now()) - (p_window_days - 1)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: an event before the window' USING ERRCODE = 'OB105', DETAIL = v_named;
    END IF;

    -- ...and no later than now plus the skew allowance.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE r.i_occurred_at > now() + p_skew_micros * interval '1 microsecond'
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: an event too far in the future'
            USING ERRCODE = 'OB106', DETAIL = v_named;
    END IF;

    -- Rating. The lock first, then a fresh snapshot sees every version written before it (see the guards above).
    IF EXISTS (SELECT 1 FROM unnest(p_meters) AS m(meter) WHERE m.meter IS NOT NULL) THEN
        LOCK TABLE ob_price_version IN ROW SHARE MODE;
        -- An id is named once, at its first copy: the copies are identical, so they rate alike.
        SELECT string_agg(x.i_event_id, ' ' ORDER BY x.i_position)
                   FILTER (WHERE x.is_first AND x.problem = 'meter') AS meter,
               string_agg(x.i_event_id, ' ' ORDER BY x.i_position)
                   FILTER (WHERE x.is_first AND x.problem = 'dimension') AS dimension,
               string_agg(x.i_event_id, ' ' ORDER BY x.i_position)
                   FILTER (WHERE x.is_first AND x.problem IS NULL AND x.amount > 9223372036854775807) AS overflow,
               array_agg(CASE WHEN x.amount <= 9223372036854775807 THEN x.amount::bigint END
                         ORDER BY x.i_position) AS amounts,
               array_agg(x.version ORDER BY x.i_position) AS versions
        INTO v_rated
        FROM (SELECT r.i_position, r.i_event_id,
                     r.i_position = min(r.i_position) OVER (PARTITION BY r.i_event_id) AS is_first,
                     p.o_problem AS problem, p.o_version AS version,
                     CASE WHEN r.i_meter IS NULL THEN r.i_amount::numeric
                          WHEN p.o_problem IS NULL
                              THEN ob_round_half_even(r.i_quantity::numeric * p.o_price_micros, p.o_per_units)
                     END AS amount
              FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                                  p_meters, p_quantities, v_dimensions, v_versions) r
              LEFT JOIN LATERAL ob_price_of(r.i_currency, r.i_meter, r.i_dimensions, r.i_occurred_at) p
                  ON r.i_meter IS NOT NULL) x;
        IF v_rated.meter IS NOT NULL THEN
            RAISE EXCEPTION 'the batch is refused: a meter with no price in the currency at that time'
                USING ERRCODE = 'OB110', DETAIL = v_rated.meter;
        END IF;
        IF v_rated.dimension IS NOT NULL THEN
            RAISE EXCEPTION 'the batch is refused: a meter with no price for these dimensions'
                USING ERRCODE = 'OB111', DETAIL = v_rated.dimension;
        END IF;
        IF v_rated.overflow IS NOT NULL THEN
            RAISE EXCEPTION 'the batch is refused: amounts that add up past the largest amount'
                USING ERRCODE = 'OB108', DETAIL = v_rated.overflow;
        END IF;
        v_amounts := v_rated.amounts;
        v_versions := v_rated.versions;
    END IF;

    -- Every event of an owner and currency whose distinct events add up past the largest amount.
    SELECT string_agg(n.i_event_id, ' ' ORDER BY n.first_position) INTO v_named
    FROM (SELECT r.i_event_id, min(r.i_position) AS first_position
          FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                              p_meters, p_quantities, v_dimensions, v_versions) r
          WHERE (r.i_owner, r.i_currency) IN (
              SELECT d.i_owner, d.i_currency
              FROM (SELECT DISTINCT u.i_event_id, u.i_owner, u.i_currency, u.i_amount
                    FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts,
                                        p_occurred_micros, p_meters, p_quantities, v_dimensions, v_versions) u) d
              GROUP BY d.i_owner, d.i_currency
              HAVING sum(d.i_amount::numeric) > 9223372036854775807)
          GROUP BY r.i_event_id) n;
    IF v_named IS NOT NULL THEN
        RAISE EXCEPTION 'the batch is refused: amounts that add up past the largest amount'
            USING ERRCODE = 'OB108', DETAIL = v_named;
    END IF;

    -- One statement writes the distinct events, in id order, or returns the rows already there.
    WITH batch AS (
        SELECT DISTINCT ON (r.i_event_id) r.*
        FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                            p_meters, p_quantities, v_dimensions, v_versions) r
        ORDER BY r.i_event_id, r.i_position
    ), written AS (
        INSERT INTO ob_usage_event AS e
            (event_id, occurred_on, occurred_at, owner, currency, member, amount, payload_hash, meter, quantity,
             dimensions, price_version)
        SELECT b.i_event_id, ob_utc_day(b.i_occurred_at), b.i_occurred_at, b.i_owner, b.i_currency, b.i_member,
               b.i_amount, b.i_payload_hash, b.i_meter, b.i_quantity, b.i_dimensions, b.i_version
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
        FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                            p_meters, p_quantities, v_dimensions, v_versions) r
        JOIN unnest(v_ids, v_inserted) AS w(event_id, inserted) ON w.event_id = r.i_event_id
        JOIN (SELECT u.i_event_id, min(u.i_position) AS first_position
              FROM ob_intake_rows(p_event_ids, p_owners, p_currencies, p_members, v_amounts, p_occurred_micros,
                                  p_meters, p_quantities, v_dimensions, v_versions) u
              GROUP BY u.i_event_id) f ON f.i_event_id = r.i_event_id
        ORDER BY r.i_position;
END
$$;

-- ---------------------------------------------------------------------------------------------------------------
-- Outcomes and the drain.

-- A plain read: `PgLedger::outcome` reads `ob_event_outcome` with a parameterized query (the owner's decision of
-- 2026-11-27: a read that carries no rule is not a function).
DROP FUNCTION ob_outcome_of(text);

DROP FUNCTION ob_drain(integer);

-- ---------------------------------------------------------------------------------------------------------------
-- The drain (ADR 0001, points 11 to 15, 22 and 23), again: a raw event's outcome keeps what rated it (ADR 0004).
-- Unchanged otherwise; replaced because its answer gains the version. `plpgsql`: it loops over the batch, and each event's draw must
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
        SELECT e.event_id, ob_day(e.occurred_on), e.state, e.refusal, e.amount, e.price_version
        FROM unnest(v_ids, v_days) WITH ORDINALITY AS b(event_id, occurred_on, pos)
        JOIN ob_usage_event e ON e.event_id = b.event_id AND e.occurred_on = b.occurred_on
        ORDER BY b.pos;
END
$$;

