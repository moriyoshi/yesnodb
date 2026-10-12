# Hand out three credentials and watch what each one can and cannot do, over a
# real socket against a real `yesnod`.
#
# # Why this is a scenario
#
# `yesno-server/tests/auth.rs` pins the grid. What is a recompile there and a
# line here is *varying the deployment*: a different anonymous mode, a role
# added, a role taken away. This file is one deployment; changing it to another
# is an edit rather than a build.
#
# # The case that matters
#
# `stats` stays on Flight while checkpoint is an administrative RPC on the
# shared control and replication listener. The test must prove both that Flight
# no longer advertises checkpoint and that the control plane applies the
# `ControlAdmin` capability.
#
#   yesno-e2e --show-output server_auth.py


def refused(fn, why):
    """Call fn, and return the message it was refused with."""
    try:
        fn()
    except RuntimeError as e:
        return str(e)
    assert False, why


cfg = srv_config("authnode", shards=2)
srv_principal(cfg, "analytics", "reader", "r-tok")
srv_principal(cfg, "loader", "writer", "w-tok")
srv_principal(cfg, "ops", "admin", "a-tok")
srv_principal(cfg, "standby-b", "replica", "s-tok")
# A second administrator, so one pair of callers differs only in identity.
srv_principal(cfg, "ops-b", "admin", "a2-tok")
srv_anonymous(cfg, "none")
srv_control_admin(cfg, "ops")
srv = srv_launch(cfg)

# ---- an administrator seeds some data to read
admin = srv_flight_as(srv, "a-tok")
want = set()
keys = []
ords = []
for i in range(500):
    o = i * 3
    keys.append(1)
    ords.append(o)
    want.add(o)
assert flight_put(admin, keys, ords) == 500

# ---- the reader
reader = srv_flight_as(srv, "r-tok")
assert flight_info(reader, 1)["total_records"] == len(want)
assert flight_get(reader, 1) == sorted(want)
# `stats` is a read, and it must reach a reader.
assert flight_action(reader, "stats")["shards"] == 2

msg = refused(lambda: flight_put(reader, [1], [7]), "a reader must not write")
assert "PermissionDenied" in msg, msg
assert "Reader" in msg, msg

msg = refused(
    lambda: srv_checkpoint_as(srv, "r-tok"), "a reader must not force a checkpoint"
)
assert "does not have permission" in msg and "ControlAdmin" in msg, msg

# ---- the writer
writer = srv_flight_as(srv, "w-tok")
assert flight_put(writer, [2], [11]) == 1
msg = refused(
    lambda: srv_checkpoint_as(srv, "w-tok"), "a writer must not force a checkpoint"
)
assert "does not have permission" in msg and "ControlAdmin" in msg, msg

# ---- the administrator
assert "compact" not in flight_actions(admin)
assert srv_checkpoint_as(srv, "a-tok") > 0

# ---- the replica credential, which is disjoint from all three
#
# It exists to ship a WAL and must not thereby be able to read a key. A
# deployment that reused `admin` for replication would give a compromised
# standby the ability to ingest and to force checkpoints.
standby = srv_flight_as(srv, "s-tok")
msg = refused(lambda: flight_info(standby, 1), "a replica credential must not read")
assert "PermissionDenied" in msg, msg
assert "Replica" in msg, msg

# ---- no credential, and a wrong one
#
# 16 and 7 are different answers and a client acts on the difference: one
# means "try a different credential", the other means "stop trying".
anon = srv_flight(srv)
msg = refused(lambda: flight_info(anon, 1), "an anonymous caller must be refused")
assert "Unauthenticated" in msg, msg

bogus = srv_flight_as(srv, "not-a-real-token")
msg = refused(lambda: flight_info(bogus, 1), "an unknown token must be refused")
assert "Unauthenticated" in msg, msg
# And **not** silently downgraded to anonymous. A caller who presents a token
# is asserting an identity; turning that into "some of your requests work" is
# far harder to diagnose than a refusal.
assert "unknown bearer token" in msg, msg

# ---- a write transaction belongs to the principal that opened it
#
# Both callers here are administrators, so this is ownership and not
# authorization: each is allowed every operation it attempts, and one is
# refused only because of whose transaction it is.
ops_b = srv_flight_as(srv, "a2-tok")
txn = flight_txn_begin(admin)
assert flight_txn_stage(admin, txn, [9], [1]) == 1

# `NotFound`, not `PermissionDenied`: a stranger must not learn that the handle
# names a live transaction, or a logged handle becomes a way to enumerate other
# callers' work.
for attempt, why in [
    (lambda: flight_txn_stage(ops_b, txn, [9], [2]), "stage into"),
    (lambda: flight_txn_commit(ops_b, txn), "commit"),
    (lambda: flight_txn_abort(ops_b, txn), "abort"),
]:
    msg = refused(attempt, "an administrator could " + why + " another's transaction")
    assert "NotFound" in msg, msg

# And the attempts took nothing: the owner still commits its own work.
assert flight_txn_commit(admin, txn) > 0
assert flight_get(admin, 9) == [1]

t = srv_stop(srv)
assert t["clean"] is True, t

# ---- the deployment where ownership distinguishes nobody
#
# **The honest limit, pinned so it is not mistaken for isolation.** Ownership is
# exactly as strong as the authentication behind it. With no principals
# configured at all, `Principal::may` returns true for everything and every
# caller resolves to the one `anonymous` name -- so two unrelated clients own
# each other's transactions, and the caps on open transactions and staged rows
# are the only defences left. A reader who saw "transactions are owned" and
# deployed without principals would get none of it.
cfg = srv_config("opennode", shards=1)
srv = srv_launch(cfg)
first = srv_flight(srv)
second = srv_flight(srv)

txn = flight_txn_begin(first)
assert flight_txn_stage(first, txn, [3], [7]) == 1
# Not a bug, and not something to fix here: `second` *is* `first` as far as the
# server can tell.
assert flight_txn_commit(second, txn) > 0
assert flight_get(first, 3) == [7]

t = srv_stop(srv)
assert t["clean"] is True, t

# ---- the same database, now with anonymous reads permitted
#
# One deployment changed into another, which is the thing a Rust test pays a
# recompile for.
cfg = srv_config("authnode", shards=2)
srv_principal(cfg, "loader", "writer", "w-tok")
srv_anonymous(cfg, "read")
srv = srv_launch(cfg)

public = srv_flight(srv)
assert flight_info(public, 1)["total_records"] == len(want), (
    "an anonymous reader could not read under anonymous = read"
)
msg = refused(lambda: flight_put(public, [1], [9]), "anonymous must not write")
assert "PermissionDenied" in msg, msg

t = srv_stop(srv)
assert t["clean"] is True, t
