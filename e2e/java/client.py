# Drive the shipped Java Flight client against the daemon an operator runs.
#
# # Why this scenario exists
#
# The Python and Go clients each have an integration suite that starts a real
# daemon and talks to it over Flight. Those two caught the Flight ticket header
# widening from 40 to 48 bytes on 2026-09-30. The Java client had no equivalent:
# its only server was a fake inside its own unit tests, and that fake minted its
# ticket with the client's own `QueryTicket.HEADER_LENGTH`, so the module stayed
# green and broken for a week while the other two jobs failed outright.
#
# What landed at the time was a 48-byte hex vector mirrored across the four
# implementations. A literal vector pins a layout a vector can express. It
# cannot pin a changed refusal, a changed status code, or a schema whose
# metadata moved -- and it only pins the layout because someone remembers to
# update all four copies.
#
# # The split of responsibility
#
# `java_client()` runs a staged `main` that drives the client and prints what it
# observed; it asserts nothing. Everything below is Python's own arithmetic over
# Python's own sets, which is what makes this an oracle rather than the client
# agreeing with itself. The last section re-reads the same server through the
# harness's **Rust** Flight client: two independent implementations have to
# agree about one server's state.
#
#   yesno-e2e e2e/java/client.py

A = 7001
B = 7002
C = 7003
D = 7004

srv = srv_start("java-client", shards=2)
f = srv_flight(srv)
obs = java_client(flight_location(f))

# ---- the capability handshake the client documentation tells a caller to make
assert obs["supports_mixed_put"] is True, obs
assert obs["supports_write_transactions"] is True, obs
assert obs["shards"] == 2, obs

# ---- what the driver wrote, counted by Python
alpha = {1, 3, 5, 7, 9}
beta = {2, 4}
assert obs["insert_rows"] == len(alpha), obs
assert obs["acked_rows"] == len(beta), obs
assert obs["keys"] == [A, B], obs

# ---- planning, which fetches no ordinals
assert obs["cardinality_a"] == len(alpha), obs
assert obs["cardinality_a_or_b"] == len(alpha | beta), obs

# ---- the ticket round trip, which is the half a fake server cannot check.
# 48 is the width the cross-implementation hex vector also pins; `refetched_rows`
# is the half it cannot reach, because only a real server decides whether it
# accepts what it minted.
assert obs["ticket_bytes"] == 48, obs
assert obs["planned_version_is_acked_version"] is True, obs
assert obs["refetched_rows"] == len(alpha), obs

# ---- ordinals, decoded as roaring on the far side
assert obs["ordinals_a"] == sorted(alpha), obs
assert obs["ordinals_a_without_3_5"] == sorted(alpha - {3, 5}), obs

# ---- mixed operations in one commit, applied in the order given
beta = (beta | {6}) - {2}
for o in range(100, 104):
    beta.add(o)
assert obs["apply_rows"] == 3, obs
assert obs["apply_advanced_version"] is True, obs
assert obs["ordinals_b"] == sorted(beta), obs

# ---- a write transaction, and the invisibility of what it has staged
gamma = {11, 13}
assert obs["staged_rows"] == len(gamma), obs
assert obs["ordinals_c_staged"] == [], obs
assert obs["commit_advanced_version"] is True, obs
assert obs["ordinals_c_committed"] == sorted(gamma), obs

# ---- and one thrown away, which must leave nothing
assert obs["ordinals_d_aborted"] == [], obs

# ---- the refusals. A status code is the half of the contract a wire vector
# cannot express, and the half that changes without anyone noticing.
assert obs["short_ticket_status"] == "INVALID_ARGUMENT", obs
# Committing twice is documented as idempotent within one live service: the
# retry finds its own outcome and returns the original version. An error here
# would mean a change-data pipe cannot resume after an ambiguous network
# failure without risking double application.
assert obs["recommit_status"] == "OK", obs
# **Measured, and not what the server said.** Aborting an already-committed
# transaction is `FAILED_PRECONDITION` on the wire; a Java caller sees
# `INVALID_ARGUMENT`, because Arrow Java's `FlightStatusCode` has no
# `FAILED_PRECONDITION` and `StatusUtils` folds it into the nearest one it has.
#
# The consequence is the line above: **this refusal and a malformed ticket are
# the same code to a Java caller.** One means "your request was nonsense" and
# the other means "your request was well formed and the world had moved on",
# and a client cannot tell them apart without parsing the message text. That is
# worth knowing before writing retry logic against either, and it is exactly
# the class of thing the 48-byte hex vector cannot express -- which is why this
# scenario exists. Pinned rather than hidden: if Arrow ever grows the code, or
# the server changes which one it sends, this line fails and someone reads it.
assert obs["abort_after_commit_status"] == "INVALID_ARGUMENT", obs
assert obs["short_ticket_status"] == obs["abort_after_commit_status"], (
    "the two refusals are expected to be indistinguishable to a Java caller; "
    "if they have diverged, the comment above is stale: %s" % obs
)

# ---- the independent half: the same server, read through the Rust client
assert flight_get(f, A) == sorted(alpha)
assert flight_get(f, B) == sorted(beta)
assert flight_get(f, C) == sorted(gamma)
assert flight_get(f, D) == []
assert flight_info(f, A)["total_records"] == len(alpha)

# ---- and the daemon still gives the database back after all of that.
#
# Worth asserting here and not only in `server_lifecycle.py`: this is the one
# scenario where a *second process* has held streams open against the daemon,
# and a `Db` handle retained on a dropped `do_get` would show up as a lock the
# daemon cannot release.
#
# This is also the only place the reader question has a settled answer. The
# driver used to read `stats().liveReaders()` before disconnecting and saw
# three, with nothing leaked: a client cannot observe the teardown of streams
# it has just closed, because the server unwinds those tasks after answering.
# `readers_left` is taken after the daemon has drained.
flight_stop(f)
t = srv_stop(srv)
assert t["clean"] is True, t
assert t["readers_left"] == 0, t
assert t["lock_released"] is True, t
