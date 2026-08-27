# Does one READ COMMITTED statement hold one yesno version across two scans?
#
# B owns an advisory lock. A materializes its first scan, then waits on that
# lock before its second scan. The DO block does not guess with a delay: it
# observes A's ungranted lock request before B commits row 4 and releases A.
#
# The first A statement errors after its scan. PostgreSQL may bypass ExecutorEnd
# on that path, so row 4 proves the transaction callback discarded the ticket.
# Without statement reuse later, first_rows is {1,2,3,4} and second_rows includes
# 5. Agreement is the oracle; the final statement is its opposite edge, proving
# that a normal end also discarded the pin and the next statement can see row 6.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE iso_stmt ( ordinal bigint ) USING yesno_table;
B: INSERT INTO iso_stmt VALUES ( 1 ), ( 2 ), ( 3 );

# The division error happens in projection, after the table AM fetched its rows.
A: SELECT ordinal / ( ordinal - 1 ) FROM iso_stmt ORDER BY ordinal;
B: INSERT INTO iso_stmt VALUES ( 4 );
A: SELECT array_agg(ordinal ORDER BY ordinal) AS recovery_after_error FROM iso_stmt;

B: SELECT pg_advisory_lock(170018);

A&: WITH first_read AS MATERIALIZED ( SELECT array_agg(ordinal ORDER BY ordinal) AS rows FROM iso_stmt ), wait_for_writer AS MATERIALIZED ( SELECT pg_advisory_lock(170018) FROM first_read ), second_read AS MATERIALIZED ( SELECT array_agg(ordinal ORDER BY ordinal) AS rows FROM iso_stmt, wait_for_writer ) SELECT first_read.rows AS first_rows, second_read.rows AS second_rows FROM first_read, second_read;

# Wait until A has completed its first scan and is blocked before the second.
B: DO $$ BEGIN WHILE NOT EXISTS ( SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND objid = 170018 AND NOT granted ) LOOP PERFORM pg_sleep(0.01); END LOOP; END $$;
B: INSERT INTO iso_stmt VALUES ( 5 );
B: SELECT pg_advisory_unlock(170018);
A!

# A acquired a session lock when B released it; do not leak it into later specs.
A: SELECT pg_advisory_unlock(170018);

# Must include 6. A transaction-scoped READ COMMITTED pin would make the first
# assertion pass by over-pinning and fail this freshness edge.
B: INSERT INTO iso_stmt VALUES ( 6 );
A: SELECT array_agg(ordinal ORDER BY ordinal) AS next_statement FROM iso_stmt;

B: DROP TABLE iso_stmt;
B: DROP EXTENSION yesno_pg CASCADE;
