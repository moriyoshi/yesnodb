# Stand a leader up, bootstrap a follower from its image, ship the log, and ask
# whether the two hold the same sets.
#
# # Why this is a scenario and not another Rust test
#
# `yesno-server/tests/replication_*.rs` already checks the pieces: the watermark rule, the
# publisher's record-boundary cut, the identity refusal. What it cannot express
# cheaply is the *sequence an operator performs* — write, checkpoint, write
# again, serve, seed, bootstrap every shard, catch up every shard, ack, stop,
# open the replica, compare — because each variation of it is a recompile.
#
# And the comparison is the payoff. `snap_load` gives a plain Python list on
# both sides, so leader-vs-follower is `==` and the expected contents of a key
# are `sorted( ... )` of an expression this file wrote. That is an oracle. A Rust
# test comparing two `BTreeSet`s is comparing yesno against yesno.
#
# # What the shape of the leader's writes is for
#
# Three keys, and the third is the load-bearing one:
#
#   key 1, 2 — written **before** the checkpoint, so they reach the follower
#       through the physical base image.
#   key 3    — written **after** it, so it exists only as log records and can
#       only arrive through catch-up. A bootstrap that copied the image and
#       shipped nothing would satisfy every other assertion here.
#
#   yesno-e2e --show-output --arg n=200000 replication.py

SHARDS = 2
N = yn_arg("n", 20000)

lead = db_open("leader", shards=SHARDS)

db_insert_range(lead, 1, 0, N - 1)

# A stride, so the operand costs one host call rather than a Python list.
sb = sb_new()
sb_stride(sb, 5, 7, N // 7)
db_insert_set(lead, 2, sb_build(sb))

db_checkpoint(lead)

# After the checkpoint. The active generation starts at the prior logical
# end rather than at LSN zero, so this key lies above the base image's replay
# offset. That is exactly the window a bootstrap that reported the current log
# end instead would skip. Nothing else in this scenario can see that.
POST = [i * 97 for i in range(3000)]
db_insert_many(lead, 3, POST)

# ---- serve -----------------------------------------------------------------

svc = repl_serve(lead)
st = repl_status(svc)
assert st["shards"] == SHARDS, f"the leader advertises {st['shards']} shards"
assert len(st["end_lsn"]) == SHARDS, "one end LSN per shard"
assert len(st["uuid"]) == 16, "an identity, not the whole manifest"
assert sum(st["end_lsn"]) > 0, "the leader reports an empty log"

# ---- bootstrap and catch up ------------------------------------------------

rep = repl_follower("replica")

# The MANIFEST, not just the identity: it carries the shard count and the
# `vshard -> shard` map, and a follower without the map looks for keys in shards
# that do not hold them.
repl_seed(rep, svc)

shipped = 0
for s in range(SHARDS):
    off = repl_bootstrap(rep, svc, s)
    assert off <= st["end_lsn"][s], (
        f"shard {s}: the replay offset {off} is past the log it refers to "
        f"({st['end_lsn'][s]}), so it names where the leader is now rather than "
        "what the image covers"
    )
    moved = repl_catch_up(rep, svc, s, off, 0)
    assert moved["shard"] == s
    assert moved["next_lsn"] == st["end_lsn"][s], f"shard {s} did not reach the end"
    shipped = shipped + moved["records"]
    # Unconditional, and it did not used to be. A bootstrapped shard always
    # has a cursor now, because the base image replaces it wholesale and
    # `bootstrap_shard` re-bases the tracker onto the offset that image carries
    # — which is what makes re-bootstrapping a working remedy for a follower
    # whose leader checkpointed. Before that, a shard that shipped no records
    # had no cursor at all, and this scenario asserted the absence: it was
    # pinning the accident rather than the property, and the gate said so.
    assert repl_cursor(rep, s) == moved["next_lsn"], (
        f"shard {s}: the resume cursor and the reported end disagree"
    )

assert shipped > 0, "the whole catch-up applied no records"
assert repl_ack(rep, svc, 0) == 0, "the leader still thinks this follower is behind"

repl_stop(svc)

# ---- the follower applies by *recovering* ----------------------------------
#
# There is no apply path in the follower: it wrote the frames into its own log
# and this `db_open` replays them through ordinary crash recovery. One framing,
# one decoder — which is the whole argument for shipping raw WAL bytes.

r = db_open("replica")
a = db_snapshot(lead)
b = db_snapshot(r)

for k in (1, 2, 3):
    assert snap_cardinality(a, k) > 0, f"the leader wrote nothing for key {k}"
    assert snap_load(a, k) == snap_load(b, k), f"key {k} diverged after catch-up"

# And against Python's own answer, not just against the leader's.
assert snap_load(b, 1) == list(range(N)), "the contiguous range did not survive"
assert snap_load(b, 3) == sorted(POST), (
    "the post-checkpoint key never reached the replica — the image arrived and "
    "the log did not"
)

print(f"  {SHARDS} shards, {shipped} records shipped, 3 keys set-equal")

snap_release(a)
snap_release(b)
db_close(r)
db_close(lead)
