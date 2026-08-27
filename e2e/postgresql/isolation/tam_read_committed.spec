# The other direction: READ COMMITTED must NOT be pinned.
#
# A fix that pins unconditionally would pass the REPEATABLE READ spec and be
# wrong here — READ COMMITTED promises each *statement* a fresh snapshot, so
# hiding another session's commit from a later statement is a defect, not
# safety. This spec is what makes over-pinning fail rather than look like rigour.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE iso2 ( ordinal bigint ) USING yesno_table;
B: INSERT INTO iso2 VALUES ( 1 ), ( 2 ), ( 3 );

A: BEGIN;
A: SELECT count(*) AS first_read FROM iso2;

B: INSERT INTO iso2 VALUES ( 4 );

# Must be 4: at READ COMMITTED the second statement sees the newer commit.
A: SELECT count(*) AS second_read FROM iso2;
A: COMMIT;

B: DROP TABLE iso2;
B: DROP EXTENSION yesno_pg CASCADE;
