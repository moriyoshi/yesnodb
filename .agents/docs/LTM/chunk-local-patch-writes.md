# Chunk-local patch writes

Durable knowledge about `WriteBatch::patch_chunk` and `RecType::ChunkPatch`:
why the operation exists, what it measured, and the trap it was designed
around. Added 2026-09-23.

## The operation

`patch_chunk( key, prefix, clear, set )` applies `new = ( old \ clear ) union
set` to one chunk. `set` wins where the masks overlap, because `clear` is
applied first. Either mask may be empty; both empty is a no-op; a patch that
empties a chunk tombstones it rather than storing an empty container.

It composes in arrival order with every other operation on the same key --
`DeleteKey`, ranges, `store_set`, and a later patch of the same prefix. That
ordering needed no new machinery: the commit sorts by `( key, arrival index )`,
which the core already documents as *structural rather than a property of the
algorithm*.

## Why it is not `PutChunk`

**`Op::PutChunk` replaces the chunk live and its `RecType::ChunkImage` unions
on replay.** They agree only because the sole producer, `store_set`, emits a
`DeleteKey` first, so both act on an emptied key. Exposing that op for
incremental use would commit one state and recover another -- silently, and
only after a crash.

The defence is structural rather than disciplinary: **one apply routine,
`Memtable::patch_chunk`, is called by the commit path and by WAL replay.**
There is no second implementation to keep in step, so there is nothing to
drift. `store_set_replays_to_what_it_committed` remains the demonstration that
the older pairing would diverge without its leading delete.

The record carries both masks through `container::codec` rather than expanded
ordinals. That also answers the objection recorded against an image record for
`PutChunk` -- that it would be "a second encoding path to keep in step with
`codec`" -- because it *is* `codec`, whose decoder is already a fuzz target
required to return `Err` rather than panic on any input.

## Measurement

96,903-row COCO residual fixture, 512-bit rows, 24,772,541 set bits, 32 shards,
release build, ARM. Encoding runs on 20 workers outside the timed region. Each
arm checkpoints, closes, reopens and verifies every LIVE bit and all 512 bits
of every row. The WAL-only arms close and reopen without checkpointing. Single
runs, not confidence intervals.

| arm | build | WAL bytes | peak RSS | checkpointed reopen + verify | WAL-only reopen + verify |
| --- | ---: | ---: | ---: | ---: | ---: |
| point inserts, 4 commits | 2.534 s | 49,408,952 | 1,332,276 KiB | 0.587 s | 4.388 s |
| native whole-key image, 1 commit | 0.120 s | 198,218,648 | 399,676 KiB | 0.585 s | 4.451 s |
| **patch, 1024-row tiles, 95 commits** | **0.489 s** | **6,298,312** | **400,008 KiB** | 0.586 s | **0.555 s** |

Against the point writer: build **5.18x** faster, WAL **7.84x** smaller, peak
RSS **3.33x** smaller, WAL-only recovery **7.91x** faster. Against the
whole-key image: WAL **31.5x** smaller and recovery **8.02x** faster.

**The image arm is not an ingest strategy and is kept only as the ceiling on
live-apply speed.** `store_set` replaces a whole key, so calling it per tile
deletes the tiles before it.

**The recovery number is the point.** The image path's fast commit was not a
fast recovery -- it replays an ordinal-expanded record, so its 0.120 s build
costs 4.451 s to reopen. Carrying container payloads instead makes the durable
cost match the live cost.

Patch beats the point writer on build *despite taking 95 commits to its 4*.

### Tile-size sweep, checkpointed

| tile rows | commits | build | WAL bytes |
| ---: | ---: | ---: | ---: |
| 128 | 758 | 4.324 s | 6,483,952 |
| 256 | 379 | 2.143 s | 6,377,832 |
| 512 | 190 | 0.985 s | 6,324,912 |
| 1024 | 95 | 0.489 s | 6,298,312 |
| 2048 | 48 | 0.308 s | 6,285,152 |
| 4096 | 24 | 0.145 s | 6,278,432 |
| 16384 | 6 | 0.034 s | 6,273,392 |

**Build time is commit-bound, not tile-bound**: about 5.7 ms per commit across
the whole sweep, which is the fsync. WAL size is nearly flat -- 6.27 to 6.48 MB,
a 3.4% spread -- because the bytes are the container payloads, not the commit
framing.

So tile size trades **atomicity granularity against fsync count**, and costs
almost nothing in bytes either way. A tile below 128 rows cannot fill a forward
chunk and pays a commit for a partial one. The knee is 1024-2048: 1024 rows is
already 5.18x faster than the point writer at 95 commits, and 2048 buys a
further 1.6x for half the atomicity granularity. Choose by how much work a
single atomic unit should cover, not by throughput.

### Query latency under a concurrent writer

A reader thread taking a snapshot and probing LIVE membership while the build
runs:

| arm | queries | p50 | p99 | max |
| --- | ---: | ---: | ---: | ---: |
| point | 4,906,544 | 0.4 us | 0.4 us | 154.2 us |
| patch, 1024-row tiles | 1,123,902 | 0.4 us | 0.7 us | 150.0 us |

Patch's p99 is 1.75x the point writer's in relative terms and both are well
under a microsecond; the tail maximum is the same. The difference tracks commit
*rate* -- patch sustains about 161 commits/s against 1.6 -- rather than any per
query cost.

## What was not measured

The end-to-end comparison through haiiie's ordered writer and service at
matched encoder thread counts. These are direct-`Db` figures and must not be
substituted for that: there are no attributes, no metadata and no service in
this harness. The consumer owns that measurement.

## Reproduction

The harness is a standalone crate with a path dependency on `yesno-core`,
built under the agent workspace rather than shipped -- an instrument is
research and research does not ship. It reads the fixture from haiiie's scratch
directory. Arms are `point`, `direct` and `patch`; arguments are
`<arm> <dir> <rows> [tile_rows] [skip-checkpoint] [with-reader]`. The
construction is recorded here because a number without its construction cannot
be re-derived, only re-measured.
