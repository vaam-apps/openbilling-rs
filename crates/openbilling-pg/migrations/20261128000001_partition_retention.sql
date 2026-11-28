-- Partition retention (ADR 0001, point 10, amended 2026-11-28 (B8)): detach and drop the usage partitions whose whole
-- day is older than the window plus a margin. The owner's decision of 2026-11-28: "Drop after W plus a margin, default
-- 90 days, configurable"; the margin is the caller's argument, never a constant here.
--
-- `DETACH PARTITION … CONCURRENTLY` cannot run inside a transaction block, nor from a function or a procedure
-- ("cannot run inside a transaction block", "cannot be executed from a function or procedure": *verified 2026-11-28*
-- against PostgreSQL 19.0, and the 19 documentation of ALTER TABLE). So the work is three SQL calls the caller makes
-- in turn, on one connection, each in its own transaction:
--
--   1. `ob_expired_partitions` decides what may go and returns the statement that detaches it;
--   2. the caller runs that statement as a top-level command (the only step that is not a function);
--   3. `ob_drop_partition` checks again that the detached table may go, and drops it.
--
-- `ob_retention_try_lock` makes two runs, on one instance or on several, take turns: a run that does not get it does
-- nothing. A run that stops between 2 and 3 leaves a detached table (or a partition marked pending-detach), which the
-- next run finds and finishes: every step is safe to repeat.

-- The lock of the job: a session-level advisory lock, because the detach cannot be inside the transaction that would
-- hold a transaction-level one. The two keys are 'OB' and 1 (retention). Held by the connection; released by the
-- unlock function, or when the connection ends.
CREATE FUNCTION ob_retention_try_lock() RETURNS boolean
    LANGUAGE sql
    RETURN pg_try_advisory_lock(20290, 1);

CREATE FUNCTION ob_retention_unlock() RETURNS boolean
    LANGUAGE sql
    RETURN pg_advisory_unlock(20290, 1);

-- The day a partition's name says, or NULL when the name is not `ob_usage_event_YYYYMMDD` of a real day. `plpgsql`:
-- it catches the error of a date that does not exist.
CREATE FUNCTION ob_partition_day(p_name text) RETURNS date
    LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    IF p_name !~ '^ob_usage_event_[0-9]{8}$' THEN
        RETURN NULL;
    END IF;
    RETURN to_date(substr(p_name, 16), 'YYYYMMDD');
EXCEPTION WHEN datetime_field_overflow OR invalid_datetime_format THEN
    RETURN NULL;
END
$$;

-- Whether a partition holds what must not be dropped: an event still `received` (undecided), a row for a day on or
-- after the cutoff (the name lied about the range), or, for its day, an outcome still `pending`. `plpgsql`: the table
-- is named at run time.
CREATE FUNCTION ob_partition_blocked(p_name text, p_day date, p_cutoff date) RETURNS boolean
    LANGUAGE plpgsql STABLE AS $$
DECLARE
    v_blocked boolean;
BEGIN
    EXECUTE format('SELECT EXISTS (SELECT 1 FROM %I WHERE state = ''received'' OR occurred_on >= $1)', p_name)
        USING p_cutoff INTO v_blocked;
    RETURN v_blocked
        OR EXISTS (SELECT 1 FROM ob_event_outcome o WHERE o.state = 'pending' AND o.occurred_on = p_day);
END
$$;

-- The first day that must stay: a day is expired when its whole range, which ends at the start of the next day, is no
-- later than `now() - (window + margin)`. One day more than intake's own bound (point 8), which is the grace day of
-- point 10. `plpgsql`: `RAISE`.
CREATE FUNCTION ob_retention_cutoff(p_window_days integer, p_margin_days integer) RETURNS date
    LANGUAGE plpgsql STABLE AS $$
BEGIN
    IF p_window_days < 1 OR p_margin_days < 0 THEN
        RAISE EXCEPTION 'the window needs a day and the margin cannot be negative' USING ERRCODE = '22023';
    END IF;
    RETURN ob_utc_day(now() - make_interval(days => p_window_days + p_margin_days));
END
$$;

-- What may be removed now, oldest first, and the statement that does the next step: `detach` (an attached partition:
-- `DETACH PARTITION … CONCURRENTLY`), `finalize` (a partition left marked pending-detach by a detach that was cut
-- short: `DETACH PARTITION … FINALIZE`) or `drop` (already detached, no statement: `ob_drop_partition` is the step).
-- A partition that holds an undecided event or a pending outcome is never listed. A detached table is listed only if
-- it is named like a partition of ours and has our columns. `plpgsql`: it loops and builds statements with `format`.
CREATE FUNCTION ob_expired_partitions(p_window_days integer, p_margin_days integer)
    RETURNS TABLE (partition_name text, partition_day date, action text, statement text)
    LANGUAGE plpgsql STABLE AS $$
DECLARE
    v_parent regclass := 'ob_usage_event'::regclass;
    v_cutoff date := ob_retention_cutoff(p_window_days, p_margin_days);
    r        record;
BEGIN
    FOR r IN
        SELECT c.relname::text AS name,
               ob_partition_day(c.relname::text) AS day,
               i.inhparent IS NOT NULL AS attached,
               COALESCE(i.inhdetachpending, false) AS pending
        FROM pg_class c
        LEFT JOIN pg_inherits i ON i.inhrelid = c.oid AND i.inhparent = v_parent
        WHERE c.relnamespace = (SELECT p.relnamespace FROM pg_class p WHERE p.oid = v_parent)
          AND c.relkind = 'r'
          AND ob_partition_day(c.relname::text) IS NOT NULL
          AND (i.inhparent IS NOT NULL
               OR EXISTS (SELECT 1 FROM pg_attribute a
                          WHERE a.attrelid = c.oid AND a.attname = 'payload_hash' AND NOT a.attisdropped))
        ORDER BY 2
    LOOP
        CONTINUE WHEN r.day >= v_cutoff OR ob_partition_blocked(r.name, r.day, v_cutoff);
        partition_name := r.name;
        partition_day := r.day;
        IF r.attached AND r.pending THEN
            action := 'finalize';
            statement := format('ALTER TABLE %s DETACH PARTITION %I FINALIZE', v_parent, r.name);
        ELSIF r.attached THEN
            action := 'detach';
            statement := format('ALTER TABLE %s DETACH PARTITION %I CONCURRENTLY', v_parent, r.name);
        ELSE
            action := 'drop';
            statement := NULL;
        END IF;
        RETURN NEXT;
    END LOOP;
END
$$;

-- Drops one detached, expired partition and returns its day. It checks everything again, because the caller ran a
-- statement of its own in between: the name is a partition's, the table is no longer attached, its day is expired, and
-- it holds no undecided event. Anything else is refused (`OB904`) and nothing is dropped. `plpgsql`: dynamic DDL and
-- `RAISE`.
CREATE FUNCTION ob_drop_partition(p_partition text, p_window_days integer, p_margin_days integer) RETURNS date
    LANGUAGE plpgsql AS $$
DECLARE
    v_parent regclass := 'ob_usage_event'::regclass;
    v_cutoff date := ob_retention_cutoff(p_window_days, p_margin_days);
    v_day    date := ob_partition_day(p_partition);
    v_table  regclass := to_regclass(quote_ident(p_partition));
BEGIN
    IF v_day IS NULL OR v_table IS NULL THEN
        RAISE EXCEPTION 'ob_drop_partition: not a partition of the usage queue' USING ERRCODE = 'OB904';
    END IF;
    IF v_day >= v_cutoff THEN
        RAISE EXCEPTION 'ob_drop_partition: the day is inside the window plus the margin' USING ERRCODE = 'OB904';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid = v_table AND i.inhparent = v_parent) THEN
        RAISE EXCEPTION 'ob_drop_partition: the partition is still attached' USING ERRCODE = 'OB904';
    END IF;
    IF ob_partition_blocked(p_partition, v_day, v_cutoff) THEN
        RAISE EXCEPTION 'ob_drop_partition: the partition holds an undecided event' USING ERRCODE = 'OB904';
    END IF;
    EXECUTE format('DROP TABLE %I', p_partition);
    RETURN v_day;
END
$$;
