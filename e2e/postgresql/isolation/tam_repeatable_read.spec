# Does a yesno table hold a stable snapshot across statements?
#
# This is the claim the module docs made and the single-session fixtures
# could not test: a REPEATABLE READ transaction must see one version of the
# table throughout, whatever another session commits in between.
#
# A holds the transaction open; B commits underneath it.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE iso ( ordinal bigint ) USING yesno_table;
B: INSERT INTO iso VALUES ( 1 ), ( 2 ), ( 3 );

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS first_read FROM iso;

B: INSERT INTO iso VALUES ( 4 );

# Must equal first_read. If it is 4, the transaction's view moved underneath
# it and REPEATABLE READ is not being honoured.
A: SELECT count(*) AS second_read FROM iso;
A: COMMIT;

# The pin must be **released** at commit. A read-only REPEATABLE READ
# transaction buffers no writes, so nothing on the write path registers the
# transaction callback that clears it — the pin has to register it itself. If it
# does not, this next transaction reads through the stale ticket and reports 3.
A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS third_read_new_txn FROM iso;
A: COMMIT;

# Outside the transaction the new row is of course visible.
B: SELECT count(*) AS after_commit FROM iso;
B: DROP TABLE iso;
B: DROP EXTENSION yesno_pg CASCADE;
