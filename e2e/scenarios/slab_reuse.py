# A slab emptied by reclamation and then handed to a *different* size class.
#
# This is the one storage transition the rest of the suite never makes, and on
# 2026-09-15 it carried a data-corrupting bug for as long as the allocator has
# existed. `Allocator::active[class]` is the per-class bump slab, and nothing
# cleared it when a slab reached `SlabState::Free`. `new_slab_for` reuses a free
# slab **with the requesting class's geometry**, so the previous owner went on
# bump-allocating into it with its own slot size: two slot sizes indexing one
# occupancy bitmap, overlapping cells, and one chunk's payload landing where
# another's `ExtTrailer` belongs. It surfaced as `verify()` reporting an extent
# that "points at another chunk" while pointing at a perfectly healthy one.
#
# Why no existing scenario reaches it
# -----------------------------------
# Every other scenario grows a database, or churns one key while others stay
# put. Slabs fill and are never emptied, so `new_slab_for` never takes its reuse
# branch. Measured 2026-09-17 against the reversal ( see the calibration below ):
# `integrity.py`, `lifecycle.py`, `durability.py`, `mvcc.py`, `ckpt_under_reader.py`
# and `aged_state.py` all pass while the bug is present.
#
# What this does instead
# ----------------------
# Rewrites **every** key every round at a cardinality that walks across size
# classes. Rewriting supersedes the old extent; after `RECLAIM_CKPT_DELAY`
# checkpoints it is reclaimed; a slab whose last slot goes returns to `Free`;
# and because the cardinality has moved meanwhile, the class asking for it next
# is usually a different one. That is the transition.
#
# CALIBRATION -- do not shrink this without re-running it
# -------------------------------------------------------
# Measured 2026-09-17 by reverting `retire_from_active` in `free_now`
# ( `.agents-workspace/tmp/unfix-retire-from-active.patch` ) and running the grid.
#
#     keys   rounds   against the reversal
#        2        4   passes -- BLIND
#        4        4   passes -- BLIND
#        8        4   passes -- BLIND
#        2        6   catches it
#        8        6   catches it
#       16        6   catches it
#       64        6   catches it
#       32       12   catches it
#
# **`rounds` is the variable and `keys` is not.** Detection needs one full pass
# of the ladder ( 6 entries ) so that a class is actually left behind, plus the
# `RECLAIM_CKPT_DELAY` checkpoints before a superseded extent is reclaimed and
# its slab can empty. Two keys are enough; four rounds are not, at any width.
#
# That is worth stating because the backlog entry this fixture closes carried a
# consumer's calibration in which **document count** was the variable. That was
# their finding for their workload and it does not transfer: theirs needed
# volume to make slabs empty, this one makes them empty by construction and
# needs *time* instead. A calibration is a property of a fixture, not of a bug.
#
# Defaults are 32 x 12: two full ladder cycles, twice the measured threshold.
#
# Full-scale run: `cargo run -p yesno-e2e -- e2e/scenarios/slab_reuse.py --arg keys=512 --arg rounds=24`

keys = yn_arg("keys", 32)
rounds = yn_arg("rounds", 12)

d = db_open("slabreuse", shards=1)

# Cardinalities that put an Array container in different size classes.
#
# **Every one must exceed `PACK_MAX` ( 2028 bytes, so ~1014 ordinals at two
# bytes each ).** A payload at or under it goes into a shared *packed page*,
# which is a single class for everything, so a ladder of small cardinalities
# migrates nothing and the fixture is blind -- which is exactly what the first
# version of this file did: 12 to 900 ordinals, every chunk packed, passing
# against the reversal. The `extent_chunks` control below is what makes that
# visible rather than silent.
#
# Walking up *and* down matters too: a slab is freed when the class that owned
# it stops being asked for, which needs the sweep to leave a class behind.
ladder = [1100, 2200, 3600, 2200, 1100, 2900]

# CONTROL on the fixture's own parameters, because the failure it guards is a
# fixture error rather than a system one: the first version used 12-900
# ordinals, every payload fitted in a packed page, one class was exercised, and
# it passed against the reversal while claiming to migrate classes.
PACK_MAX = 2028
for n in ladder:
    assert 2 * n > PACK_MAX, (
        f"ladder entry {n} encodes to about {2 * n} bytes, at or under PACK_MAX "
        f"({PACK_MAX}), so it lands in a shared packed page and migrates no "
        f"size class -- this fixture would be blind"
    )

seen_pending = False

for r in range(rounds):
    n = ladder[r % len(ladder)]
    for k in range(keys):
        # Replace, not accumulate: the cardinality has to be able to *fall* as
        # well as rise, or no class is ever left behind and no slab empties.
        b = batch(d)
        batch_delete_key(b, k)
        batch_commit(b)
        # Scattered, so it stays an Array rather than optimizing to a Run.
        db_insert_many(d, k, [(k << 20) | (i * 7) for i in range(n)])
    db_checkpoint(d)

    report = db_fsck(d)
    assert report["consistent"], f"round {r}: inconsistent -- {report['errors']}"
    assert not report["errors"], f"round {r}: {report['errors']}"
    assert report["dangling"] == 0, f"round {r}: dangling slots {report['dangling']}"
    assert report["leaked"] == 0, f"round {r}: leaked slots {report['leaked']}"
    seen_pending = seen_pending or report["pending"] > 0

# Read every key back. `fsck` walks structure; this is the only thing that
# proves the *payloads* still resolve to the right key, which is precisely what
# an overlapping cell destroys.
# CONTROL on the system: extents must actually be superseded and awaiting
# reclamation, which is the precondition for a slab emptying at all. A run that
# never retains anything never frees a slab and never reaches the transition.
assert seen_pending, (
    "no round ever reported a pending extent, so nothing was superseded and no "
    "slab can have been emptied -- the transition under test never happened"
)

n = ladder[(rounds - 1) % len(ladder)]
snap = db_snapshot(d)
for k in range(keys):
    got = sorted(snap_load(snap, k))
    want = [(k << 20) | (i * 7) for i in range(n)]
    assert got == want, f"key {k} read back {len(got)} ordinals, wanted {len(want)}"

print(f"  {keys} keys x {rounds} rounds of class-crossing rewrites, all readable")
