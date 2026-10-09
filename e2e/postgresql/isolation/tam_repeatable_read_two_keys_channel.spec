# The two-key question again, over the plugin channel.
#
# `tam_repeatable_read_two_keys.spec` asks it over Flight, where minting at a
# fixed version means sending a `QueryRequest` that carries one and letting the
# server open `snapshot_at( version )`. The channel cannot reopen a version at
# all: minting at a fixed version means finding the pin the scope's *first*
# target published and reading through it without taking a new one.
#
# Two different mechanisms for one promise, so both are asked.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE c1 ( ordinal bigint ) USING yesno_table;
B: CREATE TABLE c2 ( ordinal bigint ) USING yesno_table;
B: INSERT INTO c1 VALUES ( 1 ), ( 2 ), ( 3 );
B: INSERT INTO c2 VALUES ( 10 ), ( 20 );

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS c1_first FROM c1;

# Which transport this is, asserted rather than assumed: a spec that silently
# ran over Flight would pass every line below while testing nothing new.
A: SELECT length( current_setting( 'yesno_pg.endpoint' ) ) AS endpoint_len;

B: INSERT INTO c2 VALUES ( 30 );

# **2.** The transaction fixed its version above, and `c2` is the same database.
A: SELECT count(*) AS c2_under_pin FROM c2;
A: SELECT count(*) AS c1_again FROM c1;
A: COMMIT;

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS c2_after FROM c2;
A: COMMIT;

B: DROP TABLE c1;
B: DROP TABLE c2;
B: DROP EXTENSION yesno_pg CASCADE;
