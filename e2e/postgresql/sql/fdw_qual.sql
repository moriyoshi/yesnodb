-- Qual pushdown: which filters move to the server, and which do not.
--
-- Every EXPLAIN here is load-bearing. A pushdown that produces the right rows
-- while still evaluating the filter locally is *correct but pointless*, and only
-- the plan can tell the two apart. The two things to read in each plan are:
--
--   * whether a `Filter:` line survives above the scan — if so, PostgreSQL is
--     still doing the work;
--   * what the `yesno:` line says — the expression actually sent.

CREATE EXTENSION yesno_pg;

CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint' );

-- Key 42: multiples of 7 below 70.
CREATE FOREIGN TABLE sevens ( ordinal bigint ) SERVER yesno OPTIONS ( key '42' );

-- ── Pushed down exactly: the Filter must be gone ────────────────────────────
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal = 21;
SELECT ordinal FROM sevens WHERE ordinal = 21;

EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal >= 35;
SELECT ordinal FROM sevens WHERE ordinal >= 35 ORDER BY ordinal;

EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal BETWEEN 14 AND 35;
SELECT ordinal FROM sevens WHERE ordinal BETWEEN 14 AND 35 ORDER BY ordinal;

EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal IN ( 0, 21, 63, 100 );
SELECT ordinal FROM sevens WHERE ordinal IN ( 0, 21, 63, 100 ) ORDER BY ordinal;

-- Operands reversed. `21 = ordinal` is the same predicate, but `35 < ordinal`
-- is `ordinal > 35` — the comparison is mirrored, not merely accepted. A missing
-- mirror silently returns the complementary range.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE 35 < ordinal;
SELECT ordinal FROM sevens WHERE 35 < ordinal ORDER BY ordinal;

-- A disjunction of supported branches lowers whole.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal < 14 OR ordinal > 56;
SELECT ordinal FROM sevens WHERE ordinal < 14 OR ordinal > 56 ORDER BY ordinal;

-- `NOT ( ordinal = 21 )` does **not** push down, and the plan below shows
-- why: PostgreSQL normalises it to `ordinal <> 21` before the wrapper ever sees
-- it, and `<>` is deliberately unrecognised. So the `Clause::Not` path is
-- reached far less often from real SQL than it looks — a negated *comparison*
-- becomes the opposite comparison, and only a negated *junction*
-- ( `NOT ( a = 1 OR a = 2 )` ) survives as a `BoolExpr`.
--
-- Recorded as a fixture rather than left as a surprise: the lowering for `<>`
-- is `AndNot( Key, point )` and is correct here, since a posting list has no
-- nulls — but it is a separate lowering that has not been written, and this
-- plan is what will change when it is.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE NOT ( ordinal = 21 );
SELECT ordinal FROM sevens WHERE NOT ( ordinal = 21 ) ORDER BY ordinal;

-- A negated junction, which does survive as a BoolExpr and does push down.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE NOT ( ordinal < 14 OR ordinal > 56 );
SELECT ordinal FROM sevens WHERE NOT ( ordinal < 14 OR ordinal > 56 ) ORDER BY ordinal;

-- ── Not pushed down: the Filter must remain ─────────────────────────────────
-- These are the safety cases. If a `Filter:` line ever disappears from one of
-- them, rows are being dropped that the query asked for.

-- An expression the walker does not recognise. Under AND it may be dropped from
-- the pushdown, but the clause must stay in the plan.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal % 2 = 0;
SELECT ordinal FROM sevens WHERE ordinal % 2 = 0 ORDER BY ordinal;

-- The dangerous one. `OR` with an unrecognised branch must not lower **at
-- all** — pushing only the left half would return a subset, and no filter above
-- the scan could recover the missing rows. So: no `yesno:` expression beyond the
-- bare key, and the Filter stays.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal = 7 OR ordinal % 2 = 1;
SELECT ordinal FROM sevens WHERE ordinal = 7 OR ordinal % 2 = 1 ORDER BY ordinal;

-- Mixed: one lowerable conjunct, one not. The lowered half narrows the scan,
-- the whole clause stays local.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sevens WHERE ordinal >= 21 AND ordinal % 2 = 0;
SELECT ordinal FROM sevens WHERE ordinal >= 21 AND ordinal % 2 = 0 ORDER BY ordinal;

-- ── An independent oracle: an ordinary heap table ───────────────────────────
-- Every row above is an **accepted** expected value, so the rows are
-- self-referential: they say what the wrapper did, not what it should do. The
-- plans are a real assertion; the rows were not.
--
-- `truth` is what key 42 holds, **stated here** rather than read back from
-- yesno — the test server seeds multiples of 7 below 70. It is an ordinary heap
-- table, so PostgreSQL evaluates every predicate below itself, with no yesno
-- involvement at all. Building it with `SELECT * FROM sevens` would make it
-- agree with the wrapper by construction.
CREATE TEMP TABLE truth ( ordinal bigint );
INSERT INTO truth SELECT generate_series( 0, 63, 7 );

-- The premise first: an unfiltered scan must match, or nothing below means
-- anything.
SELECT count(*) AS unfiltered_mismatches FROM (
            ( SELECT ordinal FROM sevens EXCEPT SELECT ordinal FROM truth )
  UNION ALL ( SELECT ordinal FROM truth  EXCEPT SELECT ordinal FROM sevens )
) d;

-- Compared as ordered arrays rather than as counts: two sets of equal size
-- can still differ. On success this prints one NOTICE; on failure it names the
-- predicate and both answers, which a count could not.
DO $$
DECLARE
    preds text[] := ARRAY[
        'ordinal = 21',
        'ordinal >= 35',
        'ordinal BETWEEN 14 AND 35',
        'ordinal IN ( 0, 21, 63, 100 )',
        '35 < ordinal',
        'ordinal < 14 OR ordinal > 56',
        'NOT ( ordinal < 14 OR ordinal > 56 )',
        'ordinal <> 21',
        'ordinal % 2 = 0',
        'ordinal = 7 OR ordinal % 2 = 1',
        'ordinal >= 21 AND ordinal % 2 = 0',
        'ordinal <= 0',
        'ordinal > 63',
        'ordinal BETWEEN 35 AND 14'
    ];
    p text;
    got bigint[];
    want bigint[];
BEGIN
    FOREACH p IN ARRAY preds LOOP
        EXECUTE format(
            'SELECT array_agg( ordinal ORDER BY ordinal ) FROM sevens WHERE %s', p )
            INTO got;
        EXECUTE format(
            'SELECT array_agg( ordinal ORDER BY ordinal ) FROM truth WHERE %s', p )
            INTO want;
        IF got IS DISTINCT FROM want THEN
            RAISE EXCEPTION 'qual mismatch on [%]: wrapper=% heap=%', p, got, want;
        END IF;
    END LOOP;
    RAISE NOTICE '% predicates agree with the heap oracle', array_length( preds, 1 );
END $$;

-- ── The same oracle across the int8 sign boundary ───────────────────────────
-- This is hazard 1, and it is the one an accepted expected value hides best.
-- Key 7 holds `{ 0, 2^63, 2^63+1, u64::MAX-1 }`, which as `bigint` is
-- `{ 0, -9223372036854775808, -9223372036854775807, -2 }`. A `BETWEEN` that
-- straddles zero in `int8` is **two** disjoint `u64` ranges; a one-range
-- lowering returns nothing at all, and "nothing" looks perfectly plausible in a
-- diff.
CREATE FOREIGN TABLE straddle ( ordinal bigint ) SERVER yesno OPTIONS ( key '7' );
CREATE TEMP TABLE truth7 ( ordinal bigint );
INSERT INTO truth7 VALUES ( 0 ), ( -9223372036854775808 ), ( -9223372036854775807 ), ( -2 );

DO $$
DECLARE
    preds text[] := ARRAY[
        'ordinal BETWEEN -5 AND 5',
        'ordinal < 0',
        'ordinal >= 0',
        'ordinal = -2',
        'ordinal IN ( 0, -2 )',
        'ordinal > -9223372036854775808',
        'ordinal <= -9223372036854775807',
        'ordinal < 0 OR ordinal = 0',
        'ordinal BETWEEN -9223372036854775808 AND -1'
    ];
    p text;
    got bigint[];
    want bigint[];
BEGIN
    FOREACH p IN ARRAY preds LOOP
        EXECUTE format(
            'SELECT array_agg( ordinal ORDER BY ordinal ) FROM straddle WHERE %s', p )
            INTO got;
        EXECUTE format(
            'SELECT array_agg( ordinal ORDER BY ordinal ) FROM truth7 WHERE %s', p )
            INTO want;
        IF got IS DISTINCT FROM want THEN
            RAISE EXCEPTION 'straddle mismatch on [%]: wrapper=% heap=%', p, got, want;
        END IF;
    END LOOP;
    RAISE NOTICE '% straddling predicates agree with the heap oracle',
        array_length( preds, 1 );
END $$;

-- ── A column that is not the ordinal ────────────────────────────────────────
-- `ordinal` is resolved by **name and type**, not by position. A qual on `a`
-- must never be pushed as if it were a qual on `ordinal`.
CREATE FOREIGN TABLE two_cols ( a bigint, ordinal bigint ) SERVER yesno
    OPTIONS ( key '42' );
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM two_cols WHERE a = 21;

DROP EXTENSION yesno_pg CASCADE;
