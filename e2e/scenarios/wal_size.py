# WAL bytes per ordinal, by write shape.
#
# Was `yesno-core/examples/wal_size.rs` until 2026-08-26. The commit path turns
# every `Op` into its own record, and this measures what that costs on the three
# shapes the design names: a contiguous bulk range, scattered ordinals inside
# one chunk, and a wide random scatter.
#
# The example measured the `.wal` **files** on disk; this reads `wal_bytes`,
# which is the log's own accounting. Those are the same quantity only if the log
# is actually writing what it counts, so the round trip below checks that: every
# shape is replayed from its WAL after a reopen without a checkpoint, and the
# set that comes back must be the one that went in.
#
#   yesno-e2e --show-output --arg n=100000 wal_size.py

N = yn_arg("n", 10000)


def shape(name, base, step):
    # One shard, so `wal_bytes` is one log rather than a sum over eight and the
    # per-ordinal figure means something.
    d = db_open(name, shards=1)
    sb = sb_new()
    sb_stride(sb, base, step, N)
    ords = sb_build(sb)
    want = set_len(ords)

    before = db_stats(d)["wal_bytes"]
    written = db_insert_set(d, 1, ords)
    bytes_after = db_stats(d)["wal_bytes"]
    grew = bytes_after - before

    assert written == want, f"{name}: inserted {written} of {want}"
    # The guard the example carried for the same reason: a walker that measured
    # nothing would report a beautiful 0.0 bytes per ordinal.
    assert grew > 0, f"{name}: the WAL did not grow at all"

    print(f"  {name:<22} {want:>8} ordinals  {grew:>10} B  {grew / want:>7.1f} B/ordinal")

    # Reopen **without** checkpointing. That is the only way to prove the WAL
    # holds the writes rather than merely having grown by the right number of
    # bytes — after a checkpoint the answer would come from the store and the
    # log would be irrelevant.
    d = db_reopen(d)
    s = db_snapshot(d)
    assert snap_cardinality(s, 1) == want, f"{name}: WAL replay lost ordinals"
    back = snap_load_set(s, 1)
    assert set_len(set_xor(back, ords)) == 0, f"{name}: WAL replay changed the set"
    snap_release(s)
    db_close(d)
    return grew / want


print("WAL bytes per ordinal by write shape:")
contiguous = shape("contiguous", 0, 1)
one_chunk = shape("one-chunk-scatter", 0, 6)
wide = shape("wide-scatter", 0, 7919)

# The ordering the design predicts, and the reason the shapes are here at all:
# a contiguous run collapses into range records, a scatter inside one chunk
# still shares chunk headers, and a wide scatter pays per chunk.
assert contiguous <= one_chunk, f"contiguous {contiguous} should not cost more than one-chunk {one_chunk}"
assert one_chunk <= wide, f"one-chunk {one_chunk} should not cost more than wide {wide}"
