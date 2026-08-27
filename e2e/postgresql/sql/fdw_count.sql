-- `count(*)` pushdown: the property the wrapper exists for.
--
-- The plans are the assertion. A `count(*)` that returns the right number
-- while PostgreSQL aggregates the rows itself is *correct and worthless* — it
-- moved every ordinal across the network to add them up. What proves the
-- pushdown happened is the **absence of an `Aggregate` node**: the Foreign Scan
-- is the whole plan, and `yesno: count of …` names what the server counted.

CREATE EXTENSION yesno_pg;

CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint' );

-- Key 42: multiples of 7 below 70 ( 10 ordinals ).
CREATE FOREIGN TABLE sevens ( ordinal bigint ) SERVER yesno OPTIONS ( key '42' );
-- Key 99: never written to.
CREATE FOREIGN TABLE empty_key ( ordinal bigint ) SERVER yesno OPTIONS ( key '99' );

-- ── Pushed down ─────────────────────────────────────────────────────────────
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens;
SELECT count(*) FROM sevens;

-- `count(ordinal)` too. It differs from `count(*)` by skipping NULLs, which
-- is safe here only because the column is structurally non-null — a posting
-- list is a set of *present* values.
EXPLAIN ( COSTS OFF ) SELECT count(ordinal) FROM sevens;
SELECT count(ordinal) FROM sevens;

-- An empty key counts to zero without reading anything.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM empty_key;
SELECT count(*) FROM empty_key;

-- The composition that matters: phase 2's qual pushdown feeding phase 3's
-- count. The server counts a *filtered* set without materializing it, because
-- `Expr::cardinality` composes the operators' non-materializing walks.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens WHERE ordinal >= 35;
SELECT count(*) FROM sevens WHERE ordinal >= 35;

EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens WHERE ordinal BETWEEN 14 AND 35;
SELECT count(*) FROM sevens WHERE ordinal BETWEEN 14 AND 35;

-- ── Not pushed down ─────────────────────────────────────────────────────────
-- Each of these must keep its `Aggregate` node. They are correctness cases,
-- not missed optimisations: the server returns one number for the whole set and
-- cannot express any of these variations.

-- A qual that does not lower exactly. Unlike a row scan — where an inexact
-- pushdown is corrected by the Filter that stays in the plan — a count has
-- nothing above it to re-filter, so counting a superset would return a number
-- larger than the answer with nothing able to notice.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens WHERE ordinal % 2 = 0;
SELECT count(*) FROM sevens WHERE ordinal % 2 = 0;

-- GROUP BY: the server returns one number and cannot attribute it to groups.
EXPLAIN ( COSTS OFF ) SELECT ordinal, count(*) FROM sevens GROUP BY ordinal;

-- HAVING filters groups, which do not exist here.
EXPLAIN ( COSTS OFF ) SELECT count(*) FROM sevens HAVING count(*) > 5;

-- DISTINCT changes what is counted.
EXPLAIN ( COSTS OFF ) SELECT count(DISTINCT ordinal) FROM sevens;
SELECT count(DISTINCT ordinal) FROM sevens;

-- FILTER likewise.
EXPLAIN ( COSTS OFF ) SELECT count(*) FILTER ( WHERE ordinal > 21 ) FROM sevens;
SELECT count(*) FILTER ( WHERE ordinal > 21 ) FROM sevens;

-- Other aggregates are declined on purpose. `sum` would have to fetch every
-- ordinal anyway, so pushing it down would move the same data while adding a
-- path that can be wrong. `min`/`max` are worse: yesno's are `u64` extremes and
-- PostgreSQL wants `int8` ones, which differ for any set spanning 2^63.
EXPLAIN ( COSTS OFF ) SELECT sum(ordinal) FROM sevens;
EXPLAIN ( COSTS OFF ) SELECT min(ordinal) FROM sevens;

-- A count beside a plain column has no single value to report.
EXPLAIN ( COSTS OFF ) SELECT count(*), max(ordinal) FROM sevens;

DROP EXTENSION yesno_pg CASCADE;
