-- Foreign join pushdown: two posting lists intersected server-side.
--
-- The plan is the assertion again. A join that returns the right rows while
-- PostgreSQL hash-joins them is correct and pointless — it fetched both keys in
-- full to discard most of both. What proves the pushdown is the **absence of a
-- Hash Join**: one Foreign Scan, and `yesno:` naming the set operation.

CREATE EXTENSION yesno_pg;

CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint' );

-- Key 42: multiples of 7 below 70. Key 43: multiples of 3 below 30.
-- They share 0 and 21.
CREATE FOREIGN TABLE sevens ( ordinal bigint ) SERVER yesno OPTIONS ( key '42' );
CREATE FOREIGN TABLE threes ( ordinal bigint ) SERVER yesno OPTIONS ( key '43' );
CREATE FOREIGN TABLE empty_key ( ordinal bigint ) SERVER yesno OPTIONS ( key '99' );

-- ── Pushed down ─────────────────────────────────────────────────────────────
EXPLAIN ( COSTS OFF ) SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal );
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) ORDER BY s.ordinal;

-- Both sides projected. The scan emits one column per referenced Var and
-- fills them with the same ordinal — which is exactly what the join condition
-- asserts. Filling only the first would leave the second holding stale data.
EXPLAIN ( COSTS OFF ) SELECT s.ordinal, t.ordinal FROM sevens s JOIN threes t ON s.ordinal = t.ordinal;
SELECT s.ordinal, t.ordinal FROM sevens s JOIN threes t ON s.ordinal = t.ordinal ORDER BY 1;

-- The composition worth having: each side's own quals narrow it *before* the
-- intersection, so the server intersects two already-filtered sets.
EXPLAIN ( COSTS OFF )
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) WHERE s.ordinal > 0;
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) WHERE s.ordinal > 0 ORDER BY 1;

-- An anti-join is a set difference.
EXPLAIN ( COSTS OFF )
SELECT s.ordinal FROM sevens s WHERE NOT EXISTS ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal );
SELECT s.ordinal FROM sevens s
 WHERE NOT EXISTS ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal ) ORDER BY 1;

-- A semi-join is an intersection.
EXPLAIN ( COSTS OFF )
SELECT s.ordinal FROM sevens s WHERE EXISTS ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal );
SELECT s.ordinal FROM sevens s
 WHERE EXISTS ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal ) ORDER BY 1;

-- Intersecting with an empty key is empty, and costs no ordinals.
SELECT count(*) FROM sevens s JOIN empty_key e USING ( ordinal );

-- Join feeding count: the whole stack. No rows cross the wire at all.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens s JOIN threes t USING ( ordinal );
SELECT count(*) FROM sevens s JOIN threes t USING ( ordinal );

-- ── Not pushed down ─────────────────────────────────────────────────────────
-- Correctness cases, not missed optimisations.

-- A LEFT JOIN emits rows whose inner side is NULL. No set contains a
-- null-extended row, and the result is not a subset of either operand.
EXPLAIN ( COSTS OFF ) SELECT s.ordinal, t.ordinal FROM sevens s LEFT JOIN threes t USING ( ordinal );
SELECT s.ordinal, t.ordinal FROM sevens s LEFT JOIN threes t USING ( ordinal ) ORDER BY 1;

-- A FULL JOIN, for the same reason on both sides.
EXPLAIN ( COSTS OFF ) SELECT s.ordinal, t.ordinal FROM sevens s FULL JOIN threes t USING ( ordinal );

-- A cross join has no join clause. Pushing it as an intersection would
-- return the diagonal of a Cartesian product — a wrong answer that happens to
-- look plausible.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens s CROSS JOIN threes t;
SELECT count(*) FROM sevens s CROSS JOIN threes t;

-- A join clause that is not the ordinal equality. Leaving it unevaluated would
-- widen the intersection, and a pushed join has no Filter above it.
EXPLAIN ( COSTS OFF ) SELECT s.ordinal FROM sevens s JOIN threes t ON s.ordinal > t.ordinal;

-- One side not lowering exactly disqualifies the whole join, for the same
-- reason: the leftover clause has nowhere to live.
EXPLAIN ( COSTS OFF )
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) WHERE s.ordinal % 2 = 0;
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) WHERE s.ordinal % 2 = 0 ORDER BY 1;

-- ── An oracle: the same join, computed without the pushdown ─────────────────
-- Every row above is an **accepted** expected value, so it records what the
-- pushdown did rather than what it should do. A wrong `And` / `AndNot` lowering
-- would have been frozen into the expected file and looked like a fixture.
--
-- `OFFSET 0` is an optimisation fence: it blocks subquery pull-up, so the join
-- cannot be pushed and PostgreSQL fetches both sides and joins them itself.
-- That is the same question answered by a different engine — which is what
-- makes it an oracle rather than a restatement.
--
-- Compared as ordered arrays, not counts: two results of equal size can
-- still differ, and an intersection is exactly the shape that fails that way.
DO $$
DECLARE
    r record;
    got bigint[];
    want bigint[];
    n int := 0;
BEGIN
    FOR r IN SELECT * FROM ( VALUES
        ( 'inner',
          'SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal )',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
             JOIN ( SELECT ordinal FROM threes OFFSET 0 ) t USING ( ordinal )' ),
        ( 'semi',
          'SELECT s.ordinal FROM sevens s WHERE EXISTS
             ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal )',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s WHERE EXISTS
             ( SELECT 1 FROM ( SELECT ordinal FROM threes OFFSET 0 ) t
                WHERE t.ordinal = s.ordinal )' ),
        ( 'anti',
          'SELECT s.ordinal FROM sevens s WHERE NOT EXISTS
             ( SELECT 1 FROM threes t WHERE t.ordinal = s.ordinal )',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s WHERE NOT EXISTS
             ( SELECT 1 FROM ( SELECT ordinal FROM threes OFFSET 0 ) t
                WHERE t.ordinal = s.ordinal )' ),
        ( 'anti-reversed',
          'SELECT t.ordinal FROM threes t WHERE NOT EXISTS
             ( SELECT 1 FROM sevens s WHERE s.ordinal = t.ordinal )',
          'SELECT t.ordinal FROM ( SELECT ordinal FROM threes OFFSET 0 ) t WHERE NOT EXISTS
             ( SELECT 1 FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
                WHERE s.ordinal = t.ordinal )' ),
        ( 'inner+qual',
          'SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) WHERE s.ordinal > 0',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
             JOIN ( SELECT ordinal FROM threes OFFSET 0 ) t USING ( ordinal )
            WHERE s.ordinal > 0' ),
        ( 'three-way',
          'SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal )
             JOIN sevens u USING ( ordinal )',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
             JOIN ( SELECT ordinal FROM threes OFFSET 0 ) t USING ( ordinal )
             JOIN ( SELECT ordinal FROM sevens OFFSET 0 ) u USING ( ordinal )' ),
        ( 'empty-side',
          'SELECT s.ordinal FROM sevens s JOIN empty_key e USING ( ordinal )',
          'SELECT s.ordinal FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
             JOIN ( SELECT ordinal FROM empty_key OFFSET 0 ) e USING ( ordinal )' ),
        ( 'count-of-join',
          'SELECT count(*) FROM sevens s JOIN threes t USING ( ordinal )',
          'SELECT count(*) FROM ( SELECT ordinal FROM sevens OFFSET 0 ) s
             JOIN ( SELECT ordinal FROM threes OFFSET 0 ) t USING ( ordinal )' ),
        ( 'count-of-scan',
          'SELECT count(*) FROM sevens',
          'SELECT count(*) FROM ( SELECT ordinal FROM sevens OFFSET 0 ) q' ),
        ( 'count-with-qual',
          'SELECT count(*) FROM sevens WHERE ordinal >= 21',
          'SELECT count(*) FROM ( SELECT ordinal FROM sevens OFFSET 0 ) q WHERE ordinal >= 21' )
    ) AS v( label, pushed, local_ ) LOOP
        EXECUTE format( 'SELECT array_agg( x ORDER BY x ) FROM ( %s ) q( x )', r.pushed )
            INTO got;
        EXECUTE format( 'SELECT array_agg( x ORDER BY x ) FROM ( %s ) q( x )', r.local_ )
            INTO want;
        IF got IS DISTINCT FROM want THEN
            RAISE EXCEPTION 'join mismatch [%]: pushed=% local=%', r.label, got, want;
        END IF;
        n := n + 1;
    END LOOP;
    RAISE NOTICE '% pushed shapes agree with the unpushed plan', n;
END $$;

DROP EXTENSION yesno_pg CASCADE;
