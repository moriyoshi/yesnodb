-- The two transports must answer identically over the same data.
--
-- Not "does the channel work" but "do the channel and Flight agree", which is
-- the property a user depends on and which testing each alone cannot
-- establish. The MySQL side gets this by running one byte-exact fixture against
-- every backend; here the fixtures echo their own SQL, so a second pass would
-- need duplicate expected files. Asserting the agreement *inside* one fixture
-- is the same check with a stable oracle.
--
-- Key 7 is the reason this is worth doing. Its ordinals are 0, 2^63, 2^63+1 and
-- 2^64-2, which cross the `bigint` sign boundary: the mapping from a `u64`
-- ordinal is a reinterpretation, so three of those four arrive as negative.
-- Two transports that disagreed about that would both look plausible alone.

CREATE EXTENSION yesno_pg;

CREATE SERVER yesno_flight FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint' );
-- The same data, reached as an out-of-process peer on a Unix socket instead.
CREATE SERVER yesno_channel FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( socket :'channel_socket' );

CREATE FOREIGN TABLE f42 ( ordinal bigint ) SERVER yesno_flight OPTIONS ( key '42' );
CREATE FOREIGN TABLE f43 ( ordinal bigint ) SERVER yesno_flight OPTIONS ( key '43' );
CREATE FOREIGN TABLE f7 ( ordinal bigint ) SERVER yesno_flight OPTIONS ( key '7' );
CREATE FOREIGN TABLE c42 ( ordinal bigint ) SERVER yesno_channel OPTIONS ( key '42' );
CREATE FOREIGN TABLE c43 ( ordinal bigint ) SERVER yesno_channel OPTIONS ( key '43' );
CREATE FOREIGN TABLE c7 ( ordinal bigint ) SERVER yesno_channel OPTIONS ( key '7' );

-- The same socket a third time, with pushdown declined. A `yesnod` can be
-- configured to evaluate no expressions and says so only in its greeting, which
-- the planner has not read and cannot -- so the operator declares it here.
CREATE SERVER yesno_channel_nopush FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( socket :'channel_socket', pushdown 'off' );
CREATE FOREIGN TABLE n42 ( ordinal bigint ) SERVER yesno_channel_nopush OPTIONS ( key '42' );

-- First, that the channel returned anything at all. A zero disagreement count
-- is also what two empty scans produce, so the agreement below is only
-- meaningful beside a row count that is not zero.
SELECT ( SELECT count(*) FROM c42 ) + ( SELECT count(*) FROM c43 )
     + ( SELECT count(*) FROM c7 ) AS channel_rows;

-- Then that the two agree, in both directions and per key. `EXCEPT` on the
-- (key, ordinal) pair catches a transport that returned the right ordinals
-- under the wrong key as well as one that lost or invented a row.
WITH flight AS (
    SELECT 42 AS k, ordinal FROM f42
    UNION ALL SELECT 43, ordinal FROM f43
    UNION ALL SELECT 7, ordinal FROM f7
), channel AS (
    SELECT 42 AS k, ordinal FROM c42
    UNION ALL SELECT 43, ordinal FROM c43
    UNION ALL SELECT 7, ordinal FROM c7
)
SELECT ( SELECT count(*) FROM ( SELECT * FROM flight EXCEPT SELECT * FROM channel ) a )
     + ( SELECT count(*) FROM ( SELECT * FROM channel EXCEPT SELECT * FROM flight ) b )
       AS disagreements;

-- ── A pushed-down filter reaches both, and they still agree ─────────────────
-- Until 2026-10-09 the planner refused to lower a qual for a channel server,
-- because the channel served keys and evaluated no expressions. Both halves of
-- closing that are asserted here.
--
-- First the plan. A `yesno:` line is the expression actually sent; a `Filter:`
-- line in its place would mean PostgreSQL is doing the work and the pushdown
-- never happened, which no count below could distinguish from a pushdown that
-- worked.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM c42 WHERE ordinal = 21;

-- Then the answers, over a window with both ends inside the key so neither
-- bound is a no-op.
SELECT count(*) AS channel_window FROM c42 WHERE ordinal >= 21 AND ordinal < 50;
SELECT count(*) AS window_disagreements FROM (
      ( SELECT ordinal FROM f42 WHERE ordinal >= 21 AND ordinal < 50
        EXCEPT SELECT ordinal FROM c42 WHERE ordinal >= 21 AND ordinal < 50 )
UNION ALL ( SELECT ordinal FROM c42 WHERE ordinal >= 21 AND ordinal < 50
        EXCEPT SELECT ordinal FROM f42 WHERE ordinal >= 21 AND ordinal < 50 )
) d;

-- Key 7 again, where a pushed-down bound has to survive the `bigint` sign
-- reinterpretation: three of its four ordinals arrive negative, so `< 0` selects
-- exactly those three. An expression that lost the mapping would select none of
-- them, and would look like an empty result rather than a wrong one.
SELECT count(*) AS channel_negative FROM c7 WHERE ordinal < 0;
SELECT count(*) AS negative_disagreements FROM (
      ( SELECT ordinal FROM f7 WHERE ordinal < 0
        EXCEPT SELECT ordinal FROM c7 WHERE ordinal < 0 )
UNION ALL ( SELECT ordinal FROM c7 WHERE ordinal < 0
        EXCEPT SELECT ordinal FROM f7 WHERE ordinal < 0 )
) d;

-- ── And declining it is a declaration, not a guess ──────────────────────────
-- The same qual against the same socket, differing only in `pushdown 'off'`.
-- `yesno: key 42, unfiltered` is worded to be unmistakable: "key 42" would mean
-- a pushed expression that happens to be the bare key, and this means no
-- pushdown happened at all. The surviving `Filter:` is the other half of it.
EXPLAIN ( COSTS OFF ) SELECT ordinal FROM n42 WHERE ordinal = 21;
SELECT ordinal FROM n42 WHERE ordinal = 21;

-- Declining pushdown must not change the answer, only who computes it.
SELECT count(*) AS nopush_disagreements FROM (
      ( SELECT ordinal FROM c42 WHERE ordinal = 21
        EXCEPT SELECT ordinal FROM n42 WHERE ordinal = 21 )
UNION ALL ( SELECT ordinal FROM n42 WHERE ordinal = 21
        EXCEPT SELECT ordinal FROM c42 WHERE ordinal = 21 )
) d;

-- Dropped explicitly rather than with CASCADE: the notice CASCADE emits lists
-- every dependent object, and its wording and order are not this fixture's to
-- assert. Naming them keeps the oracle about the comparison above.
DROP FOREIGN TABLE n42, c7, c43, c42, f7, f43, f42;
DROP SERVER yesno_channel_nopush;
DROP SERVER yesno_channel;
DROP SERVER yesno_flight;
DROP EXTENSION yesno_pg;
