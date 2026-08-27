-- The FDW's read path against a real yesno Flight server.
--
-- The harness starts the same `YesnoFlightService` yesnod uses over an
-- in-memory Db. Its seed data lives in yesno-e2e PostgreSQL harness; the
-- arithmetic below is checkable by hand, which is the point of keeping the
-- sets small.

CREATE EXTENSION yesno_pg;

CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint' );

-- Key 42 holds multiples of 7 below 70; key 43 multiples of 3 below 30.
CREATE FOREIGN TABLE sevens ( ordinal bigint ) SERVER yesno OPTIONS ( key '42' );
CREATE FOREIGN TABLE threes ( ordinal bigint ) SERVER yesno OPTIONS ( key '43' );

-- Key 99 was never written to. An empty result here must be reachable and
-- distinguishable from an error — "no matches" is a fact about the data,
-- "no answer" is not.
CREATE FOREIGN TABLE empty_key ( ordinal bigint ) SERVER yesno OPTIONS ( key '99' );

-- Key 7 spans the sign boundary; see the section at the end.
CREATE FOREIGN TABLE straddle ( ordinal bigint ) SERVER yesno OPTIONS ( key '7' );

-- ── Rows actually arrive ────────────────────────────────────────────────────
SELECT ordinal FROM sevens ORDER BY ordinal;
SELECT count(*) FROM sevens;
SELECT ordinal FROM empty_key;
SELECT count(*) FROM empty_key;

-- ── The row estimate is exact, not a guess ──────────────────────────────────
-- This is the property that distinguishes a yesno foreign table from most:
-- `get_flight_info` returns `total_records` computed from container popcounts
-- in the index, without moving an ordinal. So the planner's estimate is the
-- truth. `rows=10` below is that exact count, not a default.
EXPLAIN SELECT ordinal FROM sevens;

-- ── A join between two keys ─────────────────────────────────────────────────
-- Multiples of both 7 and 3 below 30: 0 and 21.
SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal ) ORDER BY s.ordinal;

-- This plan is the record of phase 4 landing. It read `Hash Join` over two
-- Foreign Scans until join pushdown existed, and is now a single Foreign Scan
-- evaluating one intersection server-side. The two result rows above are
-- unchanged — which is the point: only the plan could tell the difference, and
-- a fixture asserting rows alone would have shown nothing either way.
EXPLAIN ( COSTS OFF ) SELECT s.ordinal FROM sevens s JOIN threes t USING ( ordinal );

-- ── The sign boundary ───────────────────────────────────────────────────────
-- Key 7 holds ordinals 0, 2^63, 2^63 + 1 and ORDINAL_MAX ( 2^64 - 2 ). The
-- bigint mapping is a bit reinterpretation, so everything at or above 2^63
-- surfaces negative.
SELECT ordinal FROM straddle ORDER BY ordinal;

-- The ordering hazard, made concrete. In u64 order these ordinals ascend
-- 0 < 2^63 < 2^63+1 < 2^64-2; in int8 order they are -9223372036854775808 <
-- -9223372036854775807 < -2 < 0. The scan declares no pathkeys precisely so
-- PostgreSQL sorts rather than trusting yesno's order — the ORDER BY above is
-- what makes the difference observable.
SELECT ordinal, ordinal::numeric + 18446744073709551616::numeric AS as_unsigned
FROM straddle WHERE ordinal < 0 ORDER BY ordinal;

DROP EXTENSION yesno_pg CASCADE;
