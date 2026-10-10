# Does a yesno *foreign table* hold a stable snapshot across statements?
#
# `tam_repeatable_read.spec` asks this of the table access method, which pins a
# ticket through `fdw::modify::pinned_ticket`. The foreign data wrapper's scan
# path calls `open_scan( &cmd )` directly and `pinned_ticket` from nowhere, so
# the answer is expected to be no -- this spec exists to say so in numbers
# before anything is changed.
#
# Two reads per statement on purpose, because they take different paths and may
# not fail together:
#
#   * `count(*)` is pushed down as an aggregate and answered by
#     `Transport::cardinality`, which takes no ticket at all.
#   * `max( ordinal )` is not pushed down, so it is an ordinary row scan through
#     `open_scan`.
#
# The server comes from the GUC through a `DO` block because the isolation
# harness passes GUCs and not psql variables, and a `SERVER` option has to be a
# literal. Key 500 is untouched by the fixture's seed, so this spec owns its
# data.

B: CREATE EXTENSION yesno_pg;
B: DO $$ BEGIN EXECUTE format( 'CREATE SERVER fdwiso FOREIGN DATA WRAPPER yesno_fdw OPTIONS ( endpoint %L )', current_setting( 'yesno_pg.endpoint' ) ); END $$;
B: CREATE FOREIGN TABLE ft ( ordinal bigint ) SERVER fdwiso OPTIONS ( key '500' );
B: INSERT INTO ft VALUES ( 1 ), ( 2 ), ( 3 );

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS count_first FROM ft;
A: SELECT max( ordinal ) AS max_first FROM ft;

B: INSERT INTO ft VALUES ( 4 );

# Both must equal the reads above: this transaction fixed its view before B's
# insert. A 4 in either line is that path reading at whatever is current.
A: SELECT count(*) AS count_second FROM ft;
A: SELECT max( ordinal ) AS max_second FROM ft;
A: COMMIT;

# And a new transaction sees the row, so the pin is released rather than stuck.
A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS count_new_txn FROM ft;
A: COMMIT;

B: SELECT count(*) AS after_commit FROM ft;
B: DELETE FROM ft;
B: DROP FOREIGN TABLE ft;
B: DROP SERVER fdwiso;
B: DROP EXTENSION yesno_pg CASCADE;
