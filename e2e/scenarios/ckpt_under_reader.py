# Checkpoint cost as a function of how long a reader has been held.
#
# Was `yesno-core/examples/ckpt_under_reader.rs` until 2026-08-26. A pinned snapshot
# blocks reclamation, so the deferred-free list grows without bound while it
# lives. The example printed the two columns and left the reader to compare
# them; the comparison is the finding, so it is asserted here.
#
# What makes this checkable at all is that the *same* churn runs twice. A
# scenario that only held a reader could see deferred extents pile up and have
# nothing to attribute it to.
#
# The default corpus is the gate-sized one. The example's own was 12 keys x 300
# ordinals over 5 rounds:
#
#   cargo run --release -p yesno-e2e -- --show-output \
#     --arg keys=12 --arg per_key=300 --arg span=50000 --arg rounds=5 \
#     e2e/scenarios/ckpt_under_reader.py

ROUNDS = yn_arg("rounds", 4)
KEYS = yn_arg("keys", 6)
PER_KEY = yn_arg("per_key", 200)
SPAN = yn_arg("span", 20000)


def churn(d, rnd):
    for key in range(KEYS):
        sb = sb_new()
        sb_stride(sb, rnd + key * 1000, 7, PER_KEY)
        db_insert_set(d, key, sb_build(sb))
    db_insert_range(d, 99, rnd * 100000, rnd * 100000 + SPAN)


def run(label, hold_reader):
    d = db_open(label, shards=1)
    churn(d, 0)
    db_checkpoint(d)

    # The pin. Taken *after* the first checkpoint so it holds a version that
    # everything below supersedes.
    reader = db_snapshot(d) if hold_reader else None

    print(f"{label}:")
    peak = 0
    for rnd in range(1, ROUNDS + 1):
        churn(d, rnd)
        t = clock_ns()
        db_checkpoint(d)
        ms = (clock_ns() - t) / 1000000.0
        st = db_stats(d)
        peak = max(peak, st["deferred_extents"])
        print(f"  round {rnd}  checkpoint {ms:>8.1f} ms   deferred {st['deferred_extents']:>7} extents   live readers {st['live_readers']}")

    st = db_stats(d)
    print(f"  allocated {st['allocated_bytes'] / 1048576.0:.1f} MiB in {st['slabs']} slab(s)")

    # fsck **before** releasing the reader: a database with a pinned version
    # must still be internally consistent, and "consistent once you let go" is a
    # much weaker property than the one the reclamation rules promise.
    report = db_fsck(d)
    assert report["consistent"], f"{label}: {report['errors']}"

    if reader is not None:
        # The reader must still see what it was pinned to, after five
        # checkpoints have reclaimed around it.
        assert snap_cardinality(reader, 0) == PER_KEY, f"{label}: the pinned reader lost its data"
        snap_release(reader)

    db_close(d)
    return peak, st["allocated_bytes"]


free_peak, free_bytes = run("no-reader", False)
print("")
held_peak, held_bytes = run("reader-held", True)

print(f"\ndeferred-extent peak: {free_peak} with no reader, {held_peak} with one held")

# The claim the example only implied. A held reader is what stops the deferred
# list draining, so its peak must be strictly higher — if the two matched, the
# three-condition reclamation would be ignoring the pin.
assert held_peak > free_peak, (
    f"holding a reader left the deferred list no larger ({held_peak} vs {free_peak}); "
    "either reclamation is ignoring the pin, or the churn is too small to show it"
)
# And the space cost is the point of caring: a pinned reader cannot be free.
assert held_bytes >= free_bytes, f"a pinned reader allocated less space ({held_bytes} vs {free_bytes})"

# Releasing the pin must let the space go. Same corpus, but the reader is
# dropped before the last checkpoint rather than after it.
d = db_open("drains", shards=1)
churn(d, 0)
db_checkpoint(d)
r = db_snapshot(d)
for rnd in range(1, ROUNDS + 1):
    churn(d, rnd)
    db_checkpoint(d)
pinned = db_stats(d)["deferred_extents"]
snap_release(r)
db_checkpoint(d)
drained = db_stats(d)["deferred_extents"]
print(f"deferred extents: {pinned} while pinned, {drained} after release")
assert drained < pinned, f"releasing the reader reclaimed nothing ({drained} vs {pinned})"
assert db_fsck(d)["consistent"]
db_close(d)
