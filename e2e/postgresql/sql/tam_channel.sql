-- The table access method over the plugin channel.
--
-- `tam.sql` runs this access method over Flight. Three things between a
-- statement and the server are transport-specific, and none of them is reached
-- by a plain read: the GUC that selects the transport, the identity the
-- per-transaction buffer flushes under, and the single-ordinal membership probe
-- a fetch by TID makes.
--
-- The channel's yesnod is seeded with keys 42, 43 and 7, which this file never
-- touches: a yesno table's key is a hash of its own relation OID.

CREATE EXTENSION yesno_pg;
SET yesno_pg.channel_socket = :'channel_socket';

CREATE TABLE sc ( ordinal bigint ) USING yesno_table;

-- ── A write must survive its own commit ─────────────────────────────────────
-- The buffer flushes at pre-commit and reopens the transport from the identity
-- it buffered under, which is the only thing still in scope by then. That
-- identity used to be a bare string the flush dialed as a gRPC endpoint
-- whatever it held, so a channel deployment buffered happily and then failed
-- to commit.
INSERT INTO sc VALUES ( 1 ), ( 2 ), ( 3 );
SELECT ordinal FROM sc ORDER BY ordinal;

-- A transaction reads its own writes through an overlay keyed by that same
-- identity.
BEGIN;
INSERT INTO sc VALUES ( 777 );
SELECT count(*) FROM sc WHERE ordinal = 777;
COMMIT;
SELECT count(*) FROM sc WHERE ordinal = 777;

-- ROLLBACK must leave the server alone.
BEGIN;
INSERT INTO sc VALUES ( 999 );
ROLLBACK;
SELECT count(*) FROM sc WHERE ordinal = 999;

DELETE FROM sc WHERE ordinal = 777;
SELECT ordinal FROM sc ORDER BY ordinal;

-- ── A fetch by TID is a membership probe ────────────────────────────────────
-- A yesno table's TID is derived rather than assigned: ordinal `o` sits at
-- block `o / 1024`, offset `o % 1024 + 1`. So ordinal 3 is `(0,4)`, and ordinal
-- 500, which this table does not hold, is `(0,501)`.
--
-- The plan is part of the assertion. A Seq Scan with a filter on `ctid` would
-- return the same rows without ever fetching one by TID, and it is that fetch
-- which must check membership rather than synthesise the row from the TID: the
-- channel evaluates no `yesno-wire` expressions, so a check written as one was
-- rejected at the transport boundary and read back as "deleted".
--
-- `enable_seqscan` is off for exactly that reason, and it is not belt and
-- braces: this table is one page, so a Seq Scan costs less than a Tid Scan and
-- the planner picks it. The first run of this fixture proved that -- the plan
-- came back `Seq Scan on sc` with `Filter: ( ctid = ... )`, and the two queries
-- below passed with the TID path never having run.
SET enable_seqscan = off;
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM sc WHERE ctid = '(0,4)';
SELECT ordinal FROM sc WHERE ctid = '(0,4)';
SELECT count(*) FROM sc WHERE ctid = '(0,501)';
RESET enable_seqscan;

-- ── Exactly one transport ───────────────────────────────────────────────────
-- Both GUCs set is reported rather than resolved by precedence, and reported
-- rather than answered as an empty table: a silent empty answer is the one
-- outcome worse than an error.
SET yesno_pg.endpoint = :'endpoint';
SELECT count(*) FROM sc;
RESET yesno_pg.endpoint;
SELECT count(*) FROM sc;

TRUNCATE sc;
SELECT count(*) FROM sc;
DROP EXTENSION yesno_pg CASCADE;
