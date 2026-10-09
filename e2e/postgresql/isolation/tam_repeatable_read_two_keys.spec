# Does one REPEATABLE READ transaction see one version across *two* keys?
#
# `tam_repeatable_read.spec` reads one table and so cannot ask this. The pin is
# recorded per target, so a second key's ticket is minted when that key is first
# touched -- which is a later statement, and therefore possibly a later version.
# A transaction that fixed its view before B's insert must not see it, whichever
# key it looks at.
#
# Two tables rather than two foreign tables because a yesno table's key is a
# hash of its own relation OID, so this is the cheapest way to get two keys on
# one server.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE k1 ( ordinal bigint ) USING yesno_table;
B: CREATE TABLE k2 ( ordinal bigint ) USING yesno_table;
B: INSERT INTO k1 VALUES ( 1 ), ( 2 ), ( 3 );
B: INSERT INTO k2 VALUES ( 10 ), ( 20 );

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS k1_first FROM k1;

B: INSERT INTO k2 VALUES ( 30 );

# **2.** A's view of the database was fixed by the read above, before B's
# insert, and `k2` is part of the same database. Three here would mean the
# transaction holds one version per key rather than one version.
A: SELECT count(*) AS k2_under_pin FROM k2;

# And k1 is still what it was, which is the property the one-table spec checks.
A: SELECT count(*) AS k1_again FROM k1;
A: COMMIT;

# After the commit the pin is gone and both are current.
A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS k2_after FROM k2;
A: COMMIT;

B: DROP TABLE k1;
B: DROP TABLE k2;
B: DROP EXTENSION yesno_pg CASCADE;
