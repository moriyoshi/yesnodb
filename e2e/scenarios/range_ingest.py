# Cost of ingesting a contiguous ordinal range, and the identity underneath it.
#
# Was `yesno-core/examples/range_ingest.rs` until 2026-08-26, where it reported
# the two rates and asserted nothing. `SetRange` describes an arbitrary range in
# one 25-byte record and `BitmapContainer::insert_range` fills a chunk in one
# masked pass, so the question is whether the path between the API and those two
# primitives uses them or walks the range one ordinal at a time.
#
# The timing is the weaker half. The assertion is that the two paths produce
# *the same set* — an `insert_range` that used the fast primitive and got the
# boundary wrong would still look fast, and the example could not have seen it.
#
#   yesno-e2e --show-output --arg n=1000000 range_ingest.py

N = yn_arg("n", 100000)

# `db_insert_range` is inclusive `[lo, hi]`; `q_range` is half-open. The
# harness mirrors both rather than smoothing them over, so this says which.
LO = 0
HI = N - 1

d = db_open(shards=1)

# One host call each, so the clock sees ingest and not the interpreter.
sb = sb_new()
sb_stride(sb, LO, 1, N)
ords = sb_build(sb)

t0 = clock_ns()
n_many = db_insert_set(d, 1, ords)
t1 = clock_ns()
n_range = db_insert_range(d, 2, LO, HI)
t2 = clock_ns()

per_ordinal_ms = (t1 - t0) / 1000000.0
as_range_ms = (t2 - t1) / 1000000.0
print(f"  {N:>9}  insert_many {per_ordinal_ms:>8.1f} ms   insert_range {as_range_ms:>8.3f} ms")

assert n_many == N, f"insert_many reported {n_many} of {N}"
assert n_range == N, f"insert_range reported {n_range} of {N}"

# The identity the example never checked: whichever primitive each path reached
# for, the two keys must hold the same ordinals.
s = db_snapshot(d)
assert snap_cardinality(s, 1) == N
assert snap_cardinality(s, 2) == N
assert snap_min(s, 2) == LO and snap_max(s, 2) == HI

a = snap_load_set(s, 1)
b = snap_load_set(s, 2)
assert set_len(set_xor(a, b)) == 0, "insert_many and insert_range disagree"

# And the range really is closed at the top: `HI` is in, `HI + 1` is not.
assert snap_contains(s, 2, HI)
assert not snap_contains(s, 2, HI + 1)


def mix(st, key):
    # Which container kinds a key is made of, and the worst run count among
    # them. `ct_kind` is the only way to see the encoding from a scenario, and
    # the encoding is the whole reason the range path is supposed to be cheap.
    #
    # **`ct_kind` cannot say what "run" means here, and the difference is the
    # whole spread of the kernel.** A container holding 2 000 intervals is also
    # a `run`, and would satisfy every assertion below — while being the
    # *slowest* shape in the crate to intersect rather than the fastest. A
    # contiguous range is supposed to be exactly one interval per chunk;
    # `ct_run_count` is the only verb that can check it, and without it this
    # scenario asserted the encoding and not the property the encoding is for.
    out = {}
    rmax = 0
    sset = snap_load_set(st, key)
    for i in range(set_chunk_count(sset)):
        p, c = set_chunk_at(sset, i)
        k = ct_kind(c)
        out[k] = out.get(k, 0) + 1
        if k == "run":
            r = ct_run_count(c)
            if r > rmax:
                rmax = r
    return out, rmax


in_memory, in_memory_runs = mix(s, 2)
snap_release(s)

# The memtable's encoding is **not** the stored one, and reading only the
# first would have made this scenario claim something false. A range ingested
# into the memtable comes back as `run` for the full chunk and `bitmap` for the
# partial tail; `optimize()` runs at checkpoint, and only the durable form is
# all runs. Both are asserted, separately, for that reason.
db_checkpoint(d)
d = db_reopen(d)
s = db_snapshot(d)
on_disk, on_disk_runs = mix(s, 2)
print(
    f"  containers: in memory {in_memory}, on disk {on_disk};"
    f" worst run count in memory {in_memory_runs}, on disk {on_disk_runs}"
)

assert "array" not in in_memory, f"a contiguous range must never be an array: {in_memory}"
assert list(on_disk.keys()) == ["run"], f"a checkpointed contiguous range must be all runs: {on_disk}"
# The assertion the kind mix above cannot make. One interval per chunk is what
# "contiguous" means, and it is the shape `run x run` intersects fastest — 32 ns
# against 2 097 for the same chunks as bitmaps. A regression that split these
# into many short intervals would keep every assertion above green.
assert on_disk_runs == 1, f"a contiguous range must be one interval per chunk, not {on_disk_runs}"
assert in_memory_runs == 1, f"and the memtable's runs are contiguous too, not {in_memory_runs}"
assert snap_cardinality(s, 2) == N, "the range did not survive the round trip"

snap_release(s)
db_close(d)
