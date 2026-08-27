# Space under sustained churn — the aged-state measurement `ARCHITECTURE.md`
# calls for as a standing gate.
#
# Was `yesno-core/examples/aged_state.rs` until 2026-08-26. Bulk-load, then
# rewrite a fixed fraction of the keys per checkpoint, and report what the space
# cost settled at.
#
# # Why fresh numbers are actively misleading here
#
# The design measured 1.31x portable Roaring fresh and 3.59x aged, and roughly
# 90% of that gap is retention the compactor and packed pages do not touch. A
# measurement that only loads and reports is reporting the number the system
# almost never exhibits.
#
# # Why slab counts rather than file size
#
# `grow_to` rounds the file up to `SEGMENT_SIZE` ( 1 GiB ), so file length is a
# step function that says nothing at these scales. Allocated slabs are the real
# unit: 2 MiB each, and what the compactor is trying to reduce. And `in_use`
# rather than the slab total, because a `Free` slab is reusable and does not
# grow the file.
#
# # What the example could not check
#
# It reported space and never looked at the data. Every row here also asserts
# that the churn *preserved* what it rewrote — each key holds exactly its last
# generation — and that `fsck` is clean afterwards. A compactor that reclaimed
# a live extent would have produced a very good number and a corrupt database.
#
# The default corpus is gate-sized. The example's own was 200 keys x 2000
# ordinals over 60 rounds, and `--sparse` is `spread` above the 65 536 chunk
# width:
#
#   cargo run --release -p yesno-e2e -- --show-output \
#     --arg keys=200 --arg per_key=2000 --arg rounds=60 \
#     e2e/scenarios/aged_state.py
#   cargo run --release -p yesno-e2e -- --show-output \
#     --arg keys=400 --arg per_key=400 --arg rounds=40 --arg spread=100000 \
#     e2e/scenarios/aged_state.py

KEYS = yn_arg("keys", 20)
PER_KEY = yn_arg("per_key", 300)
ROUNDS = yn_arg("rounds", 8)
# Gap between consecutive ordinals. Small values give a few dense chunks; a
# spread above the 65 536 chunk width gives *one ordinal per chunk*, which is
# the sparse regime the cost model is built on and where the index dominates.
SPREAD = yn_arg("spread", 3)
SLAB_SIZE = yn_const("SLAB_SIZE")
INDEX_NODE = yn_const("INDEX_NODE")
INDEX_CLASS = yn_class_for(INDEX_NODE)


def payload(key, gen):
    # `gen` shifts the ordinals so a rewrite is genuinely different data rather
    # than the same bytes landing in the same encoding.
    sb = sb_new()
    sb_stride(sb, key * 100000000 + gen, SPREAD, PER_KEY)
    return sb_build(sb)


def measure(permille, evacuate):
    d = db_open(f"aged-{permille}-{evacuate}", shards=1, evacuate=evacuate)
    for k in range(KEYS):
        db_insert_set(d, k, payload(k, 0))
    db_checkpoint(d)
    slabs_fresh = db_stats(d)["slabs"]

    # Rewrite `permille / 1000` of the keys each round, cycling through them so
    # the whole set ages rather than the same few keys absorbing every rewrite.
    per_round = max(1, (KEYS * permille + 999) // 1000)
    cursor = 0
    generation = {}
    for k in range(KEYS):
        generation[k] = 0
    for gen in range(1, ROUNDS + 1):
        for _ in range(per_round):
            k = cursor % KEYS
            cursor = cursor + 1
            b = batch(d)
            batch_delete_key(b, k)
            batch_commit(b)
            db_insert_set(d, k, payload(k, gen))
            generation[k] = gen
        db_checkpoint(d)

    # Correctness before space. A number produced by a database that lost
    # data is not a measurement of anything.
    s = db_snapshot(d)
    for k in range(KEYS):
        want = payload(k, generation[k])
        got = snap_load_set(s, k)
        assert set_len(set_xor(want, got)) == 0, f"key {k} does not hold generation {generation[k]}"
    snap_release(s)
    report = db_fsck(d)
    assert report["consistent"], f"churn left the store inconsistent: {report['errors']}"

    st = db_stats(d)
    free, in_use = db_slab_states(d)["free"], db_slab_states(d)["in_use"]
    by_class = db_slabs_by_class(d)
    index_slabs = 0
    biggest, biggest_n = 0, -1
    for cls, n in by_class:
        if cls == INDEX_CLASS:
            index_slabs = n
        # `>=`, not `>`. Every class typically holds one slab, so this is a
        # tie the whole time, and `Rust`'s `max_by_key` keeps the *last*
        # maximum. Taking the first instead reported the packed-page class
        # ( class 0, 11 of 510 slots ) where the example reports the payload
        # class ( class 8, 192 of 502 ) — same table, different subject.
        if n >= biggest_n:
            biggest, biggest_n = cls, n

    index_bytes = (st["index_nodes_written"] - st["index_nodes_freed"]) * INDEX_NODE
    live_bytes = in_use * SLAB_SIZE
    index_pct = 100.0 * index_bytes / live_bytes if live_bytes > 0 else 0.0
    live_ordinals = KEYS * PER_KEY
    bytes_per_ordinal = st["slabs"] * SLAB_SIZE / live_ordinals
    amp = st["slabs"] / slabs_fresh if slabs_fresh > 0 else 0.0

    print(f"  churn {permille / 10.0:>4.1f}% evac {evacuate}: by class {by_class}  ( index class {INDEX_CLASS} = {index_slabs} )")
    fr = db_live_fractions(d, biggest)
    used = sum([u for u, c in fr])
    cap = sum([c for u, c in fr])
    slots = " ".join([f"{u}/{c}" for u, c in fr])
    print(f"     class {biggest}: used {used} of {cap} slots  [{slots}]")

    db_close(d)
    return {
        "permille": permille,
        "evacuate": evacuate,
        "fresh": slabs_fresh,
        "aged": st["slabs"],
        "in_use": in_use,
        "free": free,
        "amp": amp,
        "bytes_per_ordinal": bytes_per_ordinal,
        "index_pct": index_pct,
        "by_class": by_class,
        "evacuated_chunks": st["evacuated_chunks"],
    }


print(f"corpus: {KEYS} keys x {PER_KEY} ordinals = {KEYS * PER_KEY} live ordinals, spread {SPREAD}, {ROUNDS} checkpoints of churn\n")
print(f"{'churn':>7}  {'evac':>4}  {'fresh':>6}  {'aged':>6}  {'live':>6}  {'amp':>6}  {'B/ordinal':>9}  {'index%':>8}")

rows = []
# A/B on the evacuation cap. `0` disables it entirely: if the rows match, the
# evacuation path is not paying for itself on this shape.
for permille in [10, 50]:
    for evacuate in [0, 2, 8]:
        r = measure(permille, evacuate)
        rows.append(r)
        print(f"{r['permille'] / 10.0:>6.1f}%  {r['evacuate']:>4}  {r['fresh']:>6}  {r['aged']:>6}  {r['in_use']:>6}  {r['amp']:>5.2f}x  {r['bytes_per_ordinal']:>9.2f}  {r['index_pct']:>7.2f}%")

print("\namp = aged slabs / fresh slabs. Rows identical across `evac` mean the")
print("evacuation path changed nothing measurable on this workload.")

# The invariants the table is only meaningful against.
for r in rows:
    assert r["fresh"] > 0, "a loaded database allocated no slabs at all — the measurement is blind"
    assert r["aged"] >= r["in_use"], "more slabs in use than allocated"
    assert r["amp"] >= 1.0, f"churn shrank the file below its fresh size ({r['amp']}x), which no reclamation path can do"

# The sparse regime's headline, and the premise `per-key-blob-for-cold-keys`
# rests on: with one ordinal per chunk, **only the index class is allocated** —
# zero payload extents, because every 1-ordinal chunk is inline in its leaf.
#
# The claim is about which classes are allocated, not about the index's share
# of bytes. Those are different numbers and it is easy to conflate them: the
# byte share here is 31-46%, because `index_bytes` counts live nodes against
# whole slabs that are two-thirds empty. Asserting on the byte share would have
# pinned an artefact of slab occupancy; asserting on the class list pins the
# structural fact.
if SPREAD > 65536:
    for r in rows:
        classes = [cls for cls, n in r["by_class"]]
        assert classes == [INDEX_CLASS], (
            f"the sparse regime should allocate only the index class {INDEX_CLASS}, got {r['by_class']}"
        )
