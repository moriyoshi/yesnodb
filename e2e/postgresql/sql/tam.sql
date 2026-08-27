-- The table access method: `CREATE TABLE … USING yesno_table`.
--
-- A yesno table is a **single-column bigint set**, not a general heap. The
-- two restrictions are asserted here rather than left to be discovered: the
-- value domain caps near 2^42 because the tuple is its own TID, and UPDATE is
-- rejected because changing the value changes the row's identity.

CREATE EXTENSION yesno_pg;
SET yesno_pg.endpoint = :'endpoint';

CREATE TABLE s ( ordinal bigint ) USING yesno_table;

-- ── Insert and scan ─────────────────────────────────────────────────────────
INSERT INTO s VALUES ( 1 ), ( 2 ), ( 3 );
SELECT ordinal FROM s ORDER BY ordinal;
SELECT count(*) FROM s;

-- A set holds each value at most once, so re-inserting is idempotent.
INSERT INTO s VALUES ( 2 );
SELECT count(*) FROM s;

-- Bulk insert exercises the multi_insert path.
INSERT INTO s SELECT generate_series( 10, 14 );
SELECT ordinal FROM s ORDER BY ordinal;

-- The row estimate is exact, from index popcounts — the same property the
-- foreign data wrapper has, reached through a different callback.
EXPLAIN SELECT ordinal FROM s;

-- ── Delete ──────────────────────────────────────────────────────────────────
DELETE FROM s WHERE ordinal = 2;
SELECT ordinal FROM s ORDER BY ordinal;

-- ROLLBACK works here too: writes buffer to pre-commit, exactly as the
-- foreign data wrapper's do — they share the buffer.
BEGIN;
INSERT INTO s VALUES ( 999 );
ROLLBACK;
SELECT count(*) FROM s;

-- ── A transaction must read its own writes ──────────────────────────────────
-- This is the case the ROLLBACK test above **cannot** catch, and the reason
-- it is written separately. Writes buffer until pre-commit, so a scan that goes
-- straight to the server sees the pre-transaction state and an INSERT followed
-- by a SELECT in one transaction returns nothing. The ROLLBACK test passes
-- either way — it asserts the row is *absent* afterwards, which is also what a
-- lost write looks like.
BEGIN;
INSERT INTO s VALUES ( 777 );
SELECT count(*) FROM s WHERE ordinal = 777;   -- 1: its own insert is visible
SELECT ordinal FROM s ORDER BY ordinal;       -- and it appears in a full scan
COMMIT;
SELECT count(*) FROM s WHERE ordinal = 777;   -- 1: still there after commit

-- And a deleted row must stop being visible to the transaction that deleted
-- it, before that delete reaches the server.
BEGIN;
DELETE FROM s WHERE ordinal = 777;
SELECT count(*) FROM s WHERE ordinal = 777;   -- 0: its own delete is visible
COMMIT;
SELECT count(*) FROM s WHERE ordinal = 777;   -- 0

-- An insert and a delete of the same value in one transaction: the delete wins,
-- because removals are applied before inserts when the buffer is flushed.
BEGIN;
INSERT INTO s VALUES ( 888 );
DELETE FROM s WHERE ordinal = 888;
SELECT count(*) FROM s WHERE ordinal = 888;   -- 0
COMMIT;
SELECT count(*) FROM s WHERE ordinal = 888;   -- 0

-- ── The domain restriction ──────────────────────────────────────────────────
-- The tuple is its own TID, so the value must fit a ( block, offset ) pair.
-- Rejected rather than truncated: a truncated value would be a different row.
INSERT INTO s VALUES ( 4398046511104 );

-- Just inside the cap is fine.
INSERT INTO s VALUES ( 4398046510079 );
SELECT count(*) FROM s;
DELETE FROM s WHERE ordinal = 4398046510079;

-- ── Rejected outright ───────────────────────────────────────────────────────
-- UPDATE changes the row's identity.
UPDATE s SET ordinal = 100 WHERE ordinal = 1;

-- Row locking needs per-tuple state this AM has nowhere to keep, and
-- pretending to lock would tell a caller its rows are held when they are not.
SELECT ordinal FROM s WHERE ordinal = 1 FOR UPDATE;

-- A NULL has no place in a set of present values.
INSERT INTO s VALUES ( NULL );

-- ── TRUNCATE ────────────────────────────────────────────────────────────────
TRUNCATE s;
SELECT count(*) FROM s;

DROP EXTENSION yesno_pg CASCADE;
