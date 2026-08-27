-- The FDW's planning path and its option validation.
--
-- This fixture asserts **plans and errors**, not rows. The scan transport is
-- not implemented yet, and the point of the phase is that the planner
-- integration can be verified without it — which is also why the last statement
-- expects an error rather than an empty result.

CREATE EXTENSION yesno_pg;

-- ── Option validation ───────────────────────────────────────────────────────
-- Each of these must fail at CREATE time. A validator that accepted them would
-- turn a typo into a silent default taking effect on the first query.

-- A server with no transport at all.
CREATE SERVER bad_none FOREIGN DATA WRAPPER yesno_fdw;

-- Both transports at once: they are different deployments, not two spellings.
CREATE SERVER bad_both FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint 'grpc://127.0.0.1:0', data_dir '/var/lib/yesno' );

-- A misspelled option name.
CREATE SERVER bad_typo FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpiont 'grpc://127.0.0.1:0' );

-- batch_rows must be a positive integer.
CREATE SERVER bad_batch FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint 'grpc://127.0.0.1:0', batch_rows '0' );

-- A dictionary column without a dictionary is almost always a misspelled
-- `dictionary`, so it is reported rather than ignored.
CREATE SERVER bad_dict FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint 'grpc://127.0.0.1:0', term_column 't' );

-- ── A valid server ──────────────────────────────────────────────────────────
CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint 'grpc://127.0.0.1:0', dictionary 'public.yesno_terms' );

-- A foreign table needs a key, and the key must be a u64 in decimal.
CREATE FOREIGN TABLE bad_nokey ( ordinal bigint ) SERVER yesno;
CREATE FOREIGN TABLE bad_key ( ordinal bigint ) SERVER yesno OPTIONS ( key '-1' );
CREATE FOREIGN TABLE bad_key2 ( ordinal bigint ) SERVER yesno OPTIONS ( key 'abc' );

-- A key in the top half of the u64 range must be accepted. This is why `key`
-- is a string option rather than a numeric one: bigint cannot hold it without
-- the same sign reinterpretation the ordinal column needs.
CREATE FOREIGN TABLE big_key ( ordinal bigint ) SERVER yesno
    OPTIONS ( key '18446744073709551614' );

CREATE FOREIGN TABLE rust_docs ( ordinal bigint ) SERVER yesno
    OPTIONS ( key '42' );

-- ── The plan, with no server running ────────────────────────────────────────
-- Nothing is listening on the endpoint above, and that is the point of this
-- fixture: planning must still succeed. `EXPLAIN` is exactly what someone runs
-- while diagnosing an unreachable server, so `GetForeignRelSize` warns and
-- falls back rather than raising. If this ever becomes an ERROR, the plan
-- becomes un-inspectable precisely when it is most needed.
--
-- COSTS OFF because the fallback estimate is not the subject and would make
-- this fixture churn against a live server.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM rust_docs;

-- The qual is **not** pushed down yet, so it must appear as a Filter above
-- the scan. When phase 2 lands, this line is what proves the qual moved: the
-- Filter disappears. A test that only checked the rows could not tell.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM rust_docs WHERE ordinal = 7;

-- ── Execution against an unreachable server ─────────────────────────────────
-- Fetching, unlike planning, **must** fail loudly. Returning zero rows
-- when the server is down is indistinguishable from a correct scan over an
-- empty key — the difference between "no matches" and "no answer" is the whole
-- point, and only one of them is a fact about the data.
SELECT ordinal FROM rust_docs;

DROP EXTENSION yesno_pg CASCADE;
