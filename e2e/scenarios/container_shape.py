# Which container shapes a corpus actually produces — the `( m, r )` question,
# as a standing measurement rather than a one-off probe.
#
# # Why this exists
#
# `run x run` intersection spans two orders of magnitude across interval count:
# 32 ns per chunk pair at one interval, 17.7 us at 1024. Two rounds of kernel
# work in August 2026 ( hoisting the per-step reads, then galloping on skew, then
# an 8x8 NEON block ) all pay above roughly 64 intervals and nothing at all below
# it. So "does that shape occur?" decides whether any of it matters, and it is a
# property of the data rather than of the analysis.
#
# It was answered once, on 2026-08-28, by a scratch probe that no longer exists.
# That is the failure mode this project keeps hitting: a number recorded
# without the instrument that produced it can be re-measured but not re-derived.
# This is the gate-resident form.
#
# # Why the stored form and not the memtable's
#
# `optimize()` runs at checkpoint, and it is `optimize()` that decides whether a
# chunk is a `Run` at all. The memtable's encoding is a different question with a
# different answer — `range_ingest.py` records the same distinction and the same
# reason. So every shape here is inserted, checkpointed and reopened before it is
# measured.
#
# # Why the shapes are strides
#
# A burst pattern is a *union of arithmetic progressions*: runs of `L` every `P`
# positions is `L` strides of step `P`. So the whole corpus is built through
# `sb_stride` and no shape needs a Python list, which the builder's own note
# explains is the thing that does not scale in a bytecode VM.
#
#   yesno-e2e --show-output --arg chunks=8 container_shape.py

CHUNKS = yn_arg("chunks", 2)
CHUNK = 65536

d = db_open()

# `(label, [(base, step, count), ...])`. Each tuple is one `sb_stride`.
#
# The burst periods are chosen so a burst plus its gap divides evenly into the
# chunk; a ragged tail would put a shorter run at every chunk boundary and the
# reported maximum would be an artifact of the generator rather than the shape.
def bursty(burst, period):
    n = (CHUNK // period) * CHUNKS
    return [(k, period, n) for k in range(burst)]


SHAPES = [
    # One contiguous stretch: the append-shaped ingest the design calls the
    # common case, and the fastest shape the run kernels see.
    ("contiguous", [(0, 1, CHUNK * CHUNKS - 1)]),
    # Bursts of adjacent ordinals — a term appearing in clusters of neighbouring
    # documents, which is what date-sorted or series-structured corpora produce.
    #
    # The periods are not interchangeable and the band is **non-monotonic in
    # burst length**. Shorter runs mean *more* intervals, and past a point the
    # run encoding stops being chosen at all. Three different gates exclude it,
    # at three different interval counts, and knowing which one binds matters
    # because only one of them is a tunable:
    #
    #   period 24 -> 2 731 intervals. Two independent exclusions: `nruns` exceeds
    #       `RUN_MAX_INTERVALS` ( 2 032 ) so the writer would refuse it, *and*
    #       `run_bytes = 2 + 4r` is 10 926 against a bitmap's 8 192, so it is
    #       simply larger. Verified by sabotage: relaxing `OPT_GAIN` does not
    #       flip this row, because the gain gate is not what is holding it.
    #   period 36 -> 1 820 intervals, 7 282 bytes. Genuinely smaller than a
    #       bitmap, but an 11.1% saving, under `OPT_GAIN`'s 12.5%. This is the
    #       row the gain gate does decide.
    #   period 40 -> 1 638 intervals, 6 554 bytes, a 20% saving. Chosen.
    #
    # Do not "fix" a row here by widening `OPT_GAIN`; that gate is what stops
    # a container oscillating between encodings on every commit.
    ("bursts of 4", bursty(4, 24)),
    ("bursts of 16", bursty(16, 40)),
    ("bursts of 64", bursty(64, 128)),
    # Isolated ordinals at three densities. Every element is its own run, so if
    # run count alone decided the encoding these would be the runniest of all.
    ("every 3rd", [(0, 3, (CHUNK // 3) * CHUNKS)]),
    ("every 10th", [(0, 10, (CHUNK // 10) * CHUNKS)]),
]


def shape_of(st, key):
    # Kind mix and the run-count spread over `Run` containers only.
    kinds = {}
    rmin = 0
    rmax = 0
    seen_run = False
    sset = snap_load_set(st, key)
    for i in range(set_chunk_count(sset)):
        p, c = set_chunk_at(sset, i)
        k = ct_kind(c)
        kinds[k] = kinds.get(k, 0) + 1
        if k == "run":
            r = ct_run_count(c)
            if not seen_run:
                rmin = r
                rmax = r
                seen_run = True
            if r < rmin:
                rmin = r
            if r > rmax:
                rmax = r
    return kinds, seen_run, rmin, rmax


print(f"  {CHUNKS} chunks per shape, stored form ( post-checkpoint )")
print(f"  {'shape':>14}  {'containers':>34}  {'run count':>14}")

results = {}
for key in range(len(SHAPES)):
    label, strides = SHAPES[key]
    sb = sb_new()
    for base, step, count in strides:
        sb_stride(sb, base, step, count)
    db_insert_set(d, key, sb_build(sb))

db_checkpoint(d)
d = db_reopen(d)
s = db_snapshot(d)

for key in range(len(SHAPES)):
    label = SHAPES[key][0]
    kinds, seen_run, rmin, rmax = shape_of(s, key)
    results[label] = (kinds, seen_run, rmin, rmax)
    span = f"{rmin}..{rmax}" if seen_run else "-"
    print(f"  {label:>14}  {str(kinds):>34}  {span:>14}")

snap_release(s)
db_close(d)

# --- what the table has to keep saying -------------------------------------
#
# These are the four claims the 2026-08-28 investigation rests on. They are
# asserted rather than eyeballed because each one, if it silently stopped being
# true, would change which kernel work is worth doing without changing any other
# test in the suite.

contiguous_kinds, contiguous_seen, _, contiguous_rmax = results["contiguous"]
assert contiguous_seen, f"a contiguous range must store as runs: {contiguous_kinds}"
assert contiguous_rmax == 1, f"and as ONE interval per chunk, not {contiguous_rmax}"

# The band exists: clustered data does produce run containers well above the
# ~64 intervals where the kernel work starts paying.
for label in ("bursts of 16", "bursts of 64"):
    kinds, seen_run, rmin, rmax = results[label]
    assert seen_run and rmax > 64, (
        f"{label}: expected a run container above 64 intervals, got {kinds} — "
        "the regime the run x run kernel work targets has vanished, and that "
        "work is now unmotivated rather than merely unmeasured"
    )

# And the band has a **top**, which is the half no analysis predicted. Bursts
# of 4 pack 2 731 intervals into a chunk, and `Run` is then *larger* than a
# bitmap outright — so the shape with the most runs in it produces none. A test
# that only checked "clustered data makes runs" would read as coverage of the
# band while describing only its bottom edge.
kinds, seen_run, rmin, rmax = results["bursts of 4"]
assert not seen_run, (
    f"bursts of 4 packs ~2731 intervals per chunk, where `run_bytes` exceeds a "
    f"bitmap outright — it must not store as a run, got {kinds} ({rmin}..{rmax})"
)

# Isolated ordinals are never runs, at any density. This is the control: it is
# what makes the line above a statement about *clustering* rather than about
# cardinality, and without it the assertion could pass on any corpus at all.
for label in ("every 3rd", "every 10th"):
    kinds, seen_run, rmin, rmax = results[label]
    assert not seen_run, (
        f"{label}: isolated ordinals must never store as runs, got {kinds} "
        f"with run counts {rmin}..{rmax}"
    )
