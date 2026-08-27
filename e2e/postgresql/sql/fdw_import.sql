-- IMPORT FOREIGN SCHEMA: names become foreign tables.
--
-- A yesno key is a u64 and the server has no notion of what one *means*, so
-- the name→key mapping lives in an ordinary table the user owns. This is the
-- callback that reads it.

CREATE EXTENSION yesno_pg;

CREATE TABLE yesno_terms ( term text PRIMARY KEY, key bigint NOT NULL );
INSERT INTO yesno_terms VALUES ( 'sevens', 42 ), ( 'threes', 43 );

CREATE SERVER yesno FOREIGN DATA WRAPPER yesno_fdw
    OPTIONS ( endpoint :'endpoint', dictionary 'public.yesno_terms' );

CREATE SCHEMA imported;
IMPORT FOREIGN SCHEMA yesno FROM SERVER yesno INTO imported;

-- The tables must be real and queryable, not merely present in the catalog.
-- Key 42 is multiples of 7 below 70; key 43 is multiples of 3 below 30.
SELECT count(*) AS sevens FROM imported.sevens;
SELECT count(*) AS threes FROM imported.threes;
SELECT ordinal FROM imported.threes ORDER BY ordinal LIMIT 4;

-- The `key` option really came from the dictionary, not from a default.
SELECT ftoptions FROM pg_foreign_table f
  JOIN pg_class c ON c.oid = f.ftrelid
 WHERE c.relname = 'sevens';

-- ── LIMIT TO and EXCEPT ─────────────────────────────────────────────────────
CREATE SCHEMA only_one;
IMPORT FOREIGN SCHEMA yesno LIMIT TO ( sevens ) FROM SERVER yesno INTO only_one;
SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'only_one' ORDER BY 1;

CREATE SCHEMA all_but;
IMPORT FOREIGN SCHEMA yesno EXCEPT ( sevens ) FROM SERVER yesno INTO all_but;
SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'all_but' ORDER BY 1;

-- ── A key at or above 2^63 ──────────────────────────────────────────────────
-- The dictionary column is `bigint`, so a key in the upper half of the u64
-- space is stored **negative** — the same reinterpretation the ordinal column
-- uses. Reading it as unsigned, or rejecting the negative, would make half
-- the key space unnameable. -1 is u64::MAX.
INSERT INTO yesno_terms VALUES ( 'high', -1 );
CREATE SCHEMA highkey;
IMPORT FOREIGN SCHEMA yesno LIMIT TO ( high ) FROM SERVER yesno INTO highkey;
SELECT ftoptions FROM pg_foreign_table f
  JOIN pg_class c ON c.oid = f.ftrelid
 WHERE c.relname = 'high';

-- ── A term that is not a bare identifier ────────────────────────────────────
-- Dictionary rows are user data that becomes SQL. A term containing a quote
-- must produce a valid quoted identifier, not a syntax error and not an early
-- end of statement. This row is why every identifier goes through
-- `quote_identifier`.
INSERT INTO yesno_terms VALUES ( 'od''d Name', 42 );
CREATE SCHEMA quoted;
IMPORT FOREIGN SCHEMA yesno LIMIT TO ( "od'd Name" ) FROM SERVER yesno INTO quoted;
SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'quoted' ORDER BY 1;
SELECT count(*) AS still_queryable FROM quoted."od'd Name";

-- ── Without a dictionary: keys name themselves ──────────────────────────────
-- Not an error. Key enumeration makes a catalogue-free import possible.
CREATE SERVER nodict FOREIGN DATA WRAPPER yesno_fdw OPTIONS ( endpoint :'endpoint' );
CREATE SCHEMA bykey;
IMPORT FOREIGN SCHEMA yesno LIMIT TO ( k42, k43 ) FROM SERVER nodict INTO bykey;
SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE n.nspname = 'bykey' ORDER BY 1;
SELECT count(*) AS k42 FROM bykey.k42;

DROP EXTENSION yesno_pg CASCADE;
