# Does a yesno table over the plugin channel hold a stable snapshot too?
#
# `tam_repeatable_read.spec` asks this over Flight, where a ticket is
# self-describing: it carries the version, so any later statement can replay it
# against a connection opened fresh. The channel has no frame for "open a
# snapshot at version V" -- the pin *is* the snapshot handle -- so the version
# lives exactly as long as something holds it.
#
# Until 2026-10-09 the only holder was the per-statement transport, and the
# second read below failed outright with "the transaction must restart". So this
# spec is not a duplicate of the Flight one: the same SQL exercises a completely
# different mechanism, and the one it exercises did not work.
#
# A holds the transaction open; B commits underneath it.

B: CREATE EXTENSION yesno_pg;
B: CREATE TABLE isoc ( ordinal bigint ) USING yesno_table;
B: INSERT INTO isoc VALUES ( 1 ), ( 2 ), ( 3 );

A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS first_read FROM isoc;

# **Which transport this is, asserted rather than assumed.** The harness picks
# it from this file's name, and a spec that silently ran over Flight would pass
# every assertion below while testing nothing new -- the same shape of false
# pass that let a `Tid Scan` fixture plan a Seq Scan two days ago.
#
# `yesno_pg.endpoint` being empty is the one machine-independent way to say "not
# Flight": the endpoint's port and the socket's path are both temporary-directory
# specific and cannot go in a byte-exact oracle. Read after the first query, so
# the extension is loaded and both GUCs are registered. Zero here plus a read
# that answered above is a complete proof: the access method found a server, and
# it was not the Flight one.
A: SELECT length( current_setting( 'yesno_pg.endpoint' ) ) AS endpoint_len;

B: INSERT INTO isoc VALUES ( 4 );

# Must equal first_read, and reaching this line with a number at all is half the
# point: it used to be an error.
A: SELECT count(*) AS second_read FROM isoc;
A: COMMIT;

# The pin is released at commit, on the same callback that clears the ticket.
# If it were not, this transaction would read through the previous one's pin and
# report 3 -- and the snapshot would still be holding a reader slot.
A: BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ;
A: SELECT count(*) AS third_read_new_txn FROM isoc;
A: COMMIT;

# Outside the transaction the new row is of course visible.
B: SELECT count(*) AS after_commit FROM isoc;
B: DROP TABLE isoc;
B: DROP EXTENSION yesno_pg CASCADE;
