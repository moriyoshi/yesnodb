# Checkpoint and Commit Lock Contention

## Summary

A concurrent writer costs a reader latency, and for four days the mechanism was misattributed. The stall is a lock a query touches **repeatedly** -- once per shard, because a query reads keys spread across every shard and a checkpoint takes the shards one at a time -- and the total wait is the total exclusive hold however it is divided. Two kernel counters settled what a purpose-built probe could not. The work that followed shortened both holds: the checkpoint's exclusive region stopped materializing the key space twice and stopped decoding leaves it would reuse, and the commit's memtable hold stopped containing disk reads at all. Commits own the latency tail; checkpoints own the worst case.

## Key Facts

- **Shard-invariance of the total stall does not rule out a lock.** It rules out a lock a query touches *once*. A query touching every shard against a checkpoint holding one shard at a time predicts exactly the observed shape: total invariant, block count proportional to shard count.
- **Voluntary context switches discriminate blocking from preemption**, are counted for free in `/proc/self/task/<tid>/status`, and need no instrument. `majflt` from `/proc/self/stat` does the same for page faults.
- **Shard count, checkpoint staggering and batch size are all tail-shape knobs, not fixes.** Each conserves the total exclusive work and moves only its division: fewer harder stalls, or more softer ones. A percentile objective and a throughput objective want opposite settings.
- The checkpoint's exclusive hold was **linear in resident size at about 0.08 us per entry**, and `Tree::build_reusing` was never the expensive part -- it reused essentially every leaf. Materializing the builder's input was.
- **`Db::commit` holds `mem.write()` across the entire batch loop by construction**, because re-taking it per record would let a reader observe half a commit. 71-80% of that hold was `disk_chunk` -- a tree descent and a verified container read, **once per row even for keys that do not exist**.
- Hoisting those reads ahead of the lock is safe under **I2** ( a published extent is immutable ): a prefetched value cannot change underneath, only become superseded, and the memtable is authoritative for anything newer. The one hazard is a concurrent *checkpoint* evicting a memtable chunk inside the window, caught by comparing the shard superblock sequence.
- **With zero checkpoints a writer still takes reader p99 from 4.38 ms to 82.61 ms.** Commits own the tail; checkpoints own the worst case ( 138 ms -> 364 ms ).
- Per-commit thread spawning cannot pay at these timescales: `std::thread::scope` costs 31.87 / 119.65 / 570.11 us for 1 / 8 / 32 threads against an exclusive sum of ~100 us. The seam is therefore a **user-supplied executor**, not a pool this crate owns.
- Lock ordering is consistent and there is exactly one pair anywhere: `mem.write()` then `store.lock()`, inside `disk_chunk`. The checkpoint path drops the store lock before taking `mem.write()`.

## Details

### The mechanism, and three reversals on the way to it

The sequencing between the read-side lock and the checkpoint region flipped three times, and not once because someone argued better.

**The page-fault hypothesis.** A reader faults on an mmap of the file a checkpoint is `fsync`ing, and the fsync stalls those faults. It fit the evidence -- total sync volume proportional to dirty data and invariant under shard count -- and it is dead. `majflt` read around each arm of the shard sweep gives **zero major faults in every arm**, including arms carrying over five seconds of aggregate excess per checkpoint:

```text
  shards   writer      p99      max   excess/ckpt   majflt
       1      yes  14.78ms  543.49ms      5615.2ms        0
       8      yes 158.34ms  519.70ms      5668.0ms        0
      32      yes 104.58ms  134.88ms      4736.0ms        0
```

The reader never waits on disk, so a mechanism requiring a fault cannot be the cause. Scope: a two-million-document corpus page-cache resident on a 121 GiB machine. An index exceeding RAM would fault and the hypothesis could revive; it is dead for anything that fits in memory.

**CPU contention, ruled out by the same kind of counter.** Contention leaves nonvoluntary context switches behind; blocking leaves voluntary ones, and both are free in `/proc/self/task/<tid>/status`. Local, 4 shards, ~130 checkpoints, three runs per arm: voluntary **0 -> ~1030** ( disjoint, tight to +/- 20 ), nonvoluntary flat. The reader blocks about eight times per checkpoint and is never preempted.

**Then the same discriminator at consumer scale resurrected the lock:**

```text
  shards  writer   excess/ckpt   voluntary   nonvoluntary
       1      no             -           0            406
       1     yes       5463.0ms         224            298
       8     yes       5672.1ms        2003            247
      32     yes       7501.0ms        3625            270
```

Voluntary switches scale with shard count while the total wait does not -- **24 ms per block at one shard, 2 ms at thirty-two**. That is the reasoning error stated plainly: the whole edifice was built on a negative inference that never held, and every measurement inside it was individually sound while the frame around it was not. It also supplies the mechanism under the tail-shape knob, which until then was an observation with nothing behind it.

### The checkpoint's exclusive hold is input materialization

Timed directly around the store lock, acquisition to release, `dirty = 100`, three runs per row:

```text
  resident   exclusive hold      iterate   carried_loop    sort    node_ids       build
     5 000   0.38 - 0.58 ms    0.17-0.23    0.05-0.09      0.01        0.02   0.08-0.12
    20 000   1.26 - 1.87 ms    0.65-0.89    0.17-0.33   0.02-0.03   0.05-0.09  0.26-0.41
    80 000   6.30 - 7.81 ms    3.40-4.15    1.46-2.23   0.09-0.14   0.20-0.32  1.02-1.51
```

`iterate` + `carried_loop` is **76%**; `build` is **17%**. Total checkpoint time stayed 15-25 ms across every row, which independently confirms the three `fsync`s are outside the lock -- they are the bulk of a checkpoint and none of the stall.

**`Tree::build_reusing` is not the problem**: it reused 1291 of about 1291 leaves at 80 000 entries. The cost was `t.iter( &*store ).collect::<Vec<_>>()` materializing the entire key space every checkpoint purely to satisfy the builder's signature, which is scanned end to end to choose leaf boundaries. Of the two things that scan produced, only one needs a scan: superseded refs for touched keys are point lookups, while `carried_refs` existed **only** because the builder demanded the whole slice. Evacuation is the one genuine scan and is already optional and capped.

One claim was withdrawn in the process: making the rebuild `O( dirty )` does **not** touch the format's promise that a snapshot is `( root, height )`. Path-copying produces a new root at the same or greater height, which is what the append-only tree already does. The real cost is **node occupancy** -- bottom-up building packs leaves full, incremental insert with splits settles near 70%.

### Fusing the input, and the regression that generalizing produced

`Tree::build_reusing` requires a complete sorted slice, and it **already calls `t.leaves( &*sink )`**, which reads and decodes every previous leaf. The key space was therefore materialized twice per checkpoint, both times inside the exclusive region. `Tree::build_updating( sink, node_size, prev, changed, superseded, reused_out )` merges against the copy the builder already holds.

The first version was measured only at `dirty = 100` and **regressed past ~1 300 dirty keys**:

```text
   dirty        before   fused ( lookups )          fixed
     100   7.76-9.12ms        2.80-3.37ms    2.09-3.32ms
   1 000   7.09-9.60ms        4.23-5.78ms    2.55-4.41ms
  10 000  7.01-11.15ms       8.75-14.46ms    4.41-4.43ms
```

The cause, measured rather than reasoned: each `Tree::get` costs ~0.7 us descending 2-3 nodes through the store mutex and the checksum cache, so **one sequential scan of every entry** had been replaced by **one random descent per touched key** -- asymptotically better, practically worse, crossing over at about 1 300 keys against a 20 000-entry scan costing ~0.9 ms.

The fix was already in the code: `build_updating`'s merge tests every previous entry against the superseded set, so the entries it drops **are** the superseded ones and their refs cost nothing to collect there. `TreeUpdate` carries them out and `Db::checkpoint` queues them for reclamation **after** `run` returns -- safe in that order because `defer_free` and `supersede_packed_chunk` only enqueue. Result is monotone: ~2.9x at 100, ~2.3x at 1 000, ~1.9x at 10 000.

Shard-count scope, resident 80 000, dirty 1 000, total exclusive hold summed across shards: before **5.80-9.56 / 6.28-8.07 / 7.37-8.88 ms**, after **4.34-5.72 / 2.74-4.54 / 5.25-5.60 ms** at 1 / 8 / 32. The `before` column is an independent reproduction of shard-invariance of the total, four orders of magnitude below the consumer's scale, which means the shape is a property of the design and not of a corpus.

### Reusing untouched leaves without decoding them

`Tree::leaves` parsed every entry of every leaf and discarded almost all of it: a leaf no key touches is reused by page id and its entries are never read. A leaf's first key is in its header and `key_at` is `O( 1 )`, so a key *range* costs two indexed reads rather than `nkeys` parses. `leaf_spans` returns ranges, `leaf_entries` decodes one leaf on demand, and `build_updating` walks spans decoding only what it must. Occupancy, height and the snapshot shape are untouched.

```text
   dirty   layout      original          fuse    fuse + spans
   1 000    block   7.09-9.60ms   2.55-4.41ms     0.99-1.74ms
  10 000    block  7.01-11.15ms   4.41-4.43ms     4.80-4.98ms
   1 000   spread            --   5.39-9.61ms     5.05-8.03ms
  10 000   spread            --  13.05-14.35ms    8.65-13.92ms
```

**The write layout matters more than the dirty count.** A fixture dirtying keys `0..dirty` touches ~16 of 1291 leaves and is the best case by construction; striding the same writes across the key space touches nearly every leaf and the win largely goes away. Block / dirty 10 000 also carries a **~10% regression** ( tight non-overlapping ranges ) where per-leaf repacking exceeds the decode saved. The cost is therefore `O( leaves + entries in touched leaves )`, degrading to `O( total entries )` when every leaf is touched.

Two test outcomes were worth more than the change. The equivalence oracle caught `build_updating` reusing a page `build_reusing` did not, and investigation showed the **old** path was wrong: it finds unchanged leaves by comparing a positional slice, so a deletion shifts every later position and it rebuilds a leaf that did not change. The assertion became a **superset** with that reasoning written at it. A second run then failed the other way and exposed a genuine bug in the new ownership rule: a leaf owned keys up to the *next leaf's first*, absorbing appended keys into the last leaf. Corrected to "up to and including its own last key".

The fast and slow paths are **semantically identical** -- the slow path decodes and then reuses anyway -- so only an allocation count separates them. Measured: **2095** allocations with the fast path, **3375** without, bound set at 2800 and verified to pass with and fail without.

### The commit hold, and what was inside it

`Db::commit` takes `shard.mem.write()` and holds it across the entire batch loop because re-taking it per record would expose half a commit. The hold is therefore `O( records in batch )` by construction, and "hold the lock for less time" is not available without changing how atomicity is provided.

What that `O( records )` contained was **disk I/O, not memtable mutation**. Each unit's memtable miss fell back to `Shard::disk_chunk`, which takes the store mutex, descends the B+tree and reads a container with checksum verification -- inside the memtable write lock, and **once per row even for a key that does not exist**:

```text
  mode        rows/commit   hold per commit   disk_chunk share
  insert               10           13.1 us              70.9%
  insert              100           71.7 us              77.6%
  insert             1000          585.7 us              80.3%
  overwrite            10           19.4 us              75.4%
  overwrite          1000          881.3 us              78.6%
```

The `( key, prefix )` set a batch needs is known from its planned units *before* the lock is taken, so resolving them outside `mem.write()` and applying under it removes the I/O entirely -- `disk_chunk` is called **zero** times under the lock in every arm, and the hold falls **2.8x to 5.0x**:

```text
  mode        rows/commit   before      after   factor
  insert               10   13.1us     3.5us     3.7x
  insert              100   71.7us    14.3us     5.0x
  insert             1000  585.7us   212.3us     2.8x
  overwrite            10   19.4us     4.5us     4.3x
  overwrite          1000  881.3us   287.3us     3.1x
```

Two protections carry it. **A cap**: the prefetch holds every container simultaneously where the apply loop read and dropped them one at a time, and `Memtable::insert_range` calls its `base` closure for every prefix the memtable lacks, including fully-covered chunks. Beyond `PREFETCH_MAX` the closure degrades to the old in-lock read, so the failure mode is the previous behaviour rather than a memory spike. **A staleness guard**: a concurrent checkpoint can persist a memtable chunk and evict it between prefetch and apply, after which the prefetched copy is *older* than the store. Detected by comparing the shard's superblock sequence, read once per shard rather than per row. A concurrent commit cannot do this, because the path holds every participant's `write` guard.

The delete path had the same defect in a path the first measurement never exercised: `ShardStore::key_prefixes` is a **range scan of the index** and ran inside `mem.write()` once per deleted key. Hoisted into the same prefetch under the same guard, 2 000 keys, 50 deletes per commit:

```text
  chunks/key   hold before   hold after   key_prefixes share before -> after
           1       49.2 us      14.5 us                    78.2% -> 23.7%
           8       92.5 us      31.8 us                    63.7% ->  8.4%
          64      531.2 us     212.4 us                    38.8% ->  1.5%
```

The residue at 64 chunks per key is `Memtable::delete_key` itself -- 3 200 tombstone insertions per commit -- which is work, not I/O.

### Commits own the tail, checkpoints own the worst case

The control arm missing from every measurement on both sides is query latency against a writer taking **zero** checkpoints:

```text
  while querying                 median       p99      worst   ckpts
  no writer                      3.97ms    4.38ms     6.79ms       0
  writer, default policy         4.24ms   83.18ms   363.76ms       2
  writer, checkpoints deferred   4.13ms   82.61ms   138.29ms       0
```

Every measurement *about checkpoints* survives this; what does not survive is the inference from "this is what a checkpoint costs" to "this is what the writer costs".

Two corrections then shrank the commit figure by an order of magnitude. The 82.61 ms was measured with **overwrites**, and an overwrite held 12x to 39x longer than a fresh insert because the consumer's writer emitted a clear for every dimension before setting the new row. For fresh inserts a concurrent writer moves reader p99 from 0.95 ms to **1.91-4.43 ms**. Since the hold is `O( records )` by construction, the only lever is emitting fewer records, and difference-emitting writes gave about **5x** on worst-case reader stall ( 262 144 documents, checkpoints deferred, 40 000 rows per arm: 27.17 -> 7.78 ms at batch 500, 485.96 -> 95.05 ms at batch 20 000 ).

### The knob family: conserved total, changing division

Four independent parameters produce the same trade, and each time the total exclusive work is unchanged and only its division moves:

- **Shard count.** One shard gives fewer, harder stalls ( p99 18 ms, max 543 ms ); thirty-two gives more, softer ones ( p99 105 ms, max 364 ms ).
- **Checkpoint staggering.**
- **Batch size.** 500-row commits at p99 1.91 ms / worst 5.47 ms against 50 000-row commits at p99 1.28 ms / worst 23.73 ms; in overwrite arms a 20 000-row commit gives a p99 of 0.89 ms, *better than no writer at all*, against a 486 ms worst case.
- **Per-shard exclusive sum**, which is invariant at about 100 us across 1 / 8 / 32 shards.

These are knobs rather than fixes, and a consumer serving a percentile objective and one serving a throughput objective want opposite settings.

### Parallelism: sound structure, wrong arithmetic

The per-shard work is already independent -- every participant's `write` guard is held simultaneously in ascending order, which is what gives I5, and the `mem.write()` guards are taken one shard at a time inside that.

```text
  shards   shards touched   per-shard hold   SUM per commit   prefetch ( outside locks )
       1              1.0        97.89 us         97.89 us                   293.68 us
       8              8.0        13.48 us        107.87 us                   254.43 us
      32             31.9         3.11 us         99.37 us                   275.47 us
```

A scanning reader waits the sum, so parallelising the apply would make it wait the max -- 7x better at 8 shards, 32x at 32. **The mechanism kills it**: `std::thread::scope`, best of five, costs 31.87 us for one thread, 119.65 us for eight and 570.11 us for thirty-two, a net loss at every shard count measured. The hoists also moved the target: the prefetch is now 254-293 us **outside every lock**, 2.5x the entire exclusive region, per-shard independent, I/O-bound and carrying no lock-ordering question -- so it parallelises better than the apply, and at 8 shards is net positive for writer latency only.

What this needs is a persistent pool, which `yesno-core` deliberately does not have ( the position is stated at `enforce_policy`: the writer *is* the checkpointer ). The resolution was the `dispatch` seam -- a user-supplied executor -- recorded in [Database APIs and Satellite Crates](./database-apis-and-satellite-crates.md). With it, the prefetch and then the apply both run through `Dispatcher`; the apply measured ~3.5x to ~5x at 8 shards over 200 to 20 000 rows per commit.

### Open direction

`checkpoint-super-floor-work-still-holds-store-lock` remains the primary item. The part released on 2026-09-14 -- three `fsync`s, ~17 ms -- measured **0.04%** of the consumer's stall; the part deliberately left inside the lock is the rest of it. The fuse and the leaf spans removed constant factors from a path still `O( total entries )`, and at consumer scale the fuse measured **under 2% where 33-47% was predicted**, which bounds the doubly-materialized key space at a small single-digit fraction of their stall rather than showing an absence.

Where the remaining time goes, measured rather than inferred: the index work is **70-82%** of the hold and payload writes are 5-20%, so moving the writes outside the lock -- available, since new extents are unreachable until the superblock flips -- would buy little. The `O( dirty )` path-copy with untouched-subtree reuse is the right target, and it trades leaf occupancy ( bottom-up packs full; incremental insert settles near 70% ) for it. That trade has been priced once -- **+2.3% worst-case query latency and 0.7% space against a 5.6 s aggregate stall**, itself an upper bound twice over -- and the price is recorded in the backlog rather than re-derived. Measure occupancy before building it, not after.

One framing correction belongs with it: shortening the exclusive region is aimed at the **364 ms worst case**, not at the 83 ms p99. A commit takes `Shard::write` plus `Shard::mem.write()` while a reader takes `mem.read()`, so a reader meets that exclusion on **every commit** rather than once per checkpoint -- which is why the two halves of this document are separate items and not one.

## Files

- `yesno-core/src/db/mod.rs` -- `Db::checkpoint`, `Db::commit`, `enforce_policy`, the shard participants and the lock order.
- `yesno-core/src/db/checkpoint.rs` -- the exclusive region, chunk-payload writing, and deferred reclamation queuing.
- `yesno-core/src/index/tree.rs` -- `build_reusing` ( kept as the differential oracle ), `build_updating`, `TreeUpdate`, `leaf_spans`, `leaf_entries`, `key_at`.
- `yesno-core/src/db/store.rs` -- `Shard::disk_chunk`, `ShardStore::key_prefixes`, superblock sequence.
- `yesno-core/src/db/batch.rs` -- planned units, the prefetch, `PREFETCH_MAX`, the staleness guard.
- `yesno-core/src/dispatch.rs` -- the executor seam the parallel arms run through.

## Test Coverage

- `build_updating_agrees_with_build_reusing` -- same tree, same height, same per-key lookups, and reuse as a **superset** with the positional-builder defect written at the assertion. Sabotage-verified against the superseded filter, the merge's trailing drain, the superseded-ref collection, and the reuse-after-decode branch.
- An `append past the end` case, an insert spanning leaf boundaries, and a delete of a never-written key positioned inside an existing leaf -- each added because a sabotage stayed green without it.
- An allocation-count test with a bound of 2800, between the measured 2095 and 3375.
- `checkpoints_concurrent_with_writers_lose_nothing` -- and, because the staleness guard fires zero times there ( the window is microseconds ), `a_checkpoint_advances_the_store_sequence` pins the thing the guard depends on, sabotage-verified by freezing the sequence.
- `a_dispatcher_that_skips_work_is_refused` and `a_concurrent_dispatcher_commits_what_the_sequential_one_does`, the latter with a non-empty-corpus control.

## Pitfalls

- **Do not infer a lock's absence from shard-invariance of a total.** State which lock a query touches how many times before reading the invariance either way.
- **A measurement can be accurate and have the wrong subject.** Timing total checkpoint time answered a different question than the item asked; timing the lock hold itself was the correction, and the two-thirds gap between them was the out-of-lock `fsync`s.
- **Vary the parameter the conclusion is about.** Three runs per cell established that the 2.2x fuse figure was stable at `dirty = 100`; nothing established that it generalized, and it reversed by `dirty = 10 000`. Sharply: the claim generalized was about asymptotics, from a regime where the asymptotically-better term had not begun costing anything.
- **A fixture dirtying a contiguous prefix is the best case for leaf reuse by construction.** Add a strided arm before claiming a number.
- **Print the achieved parameter, never the requested one.** An arm labelled `dirty = 40 000` measured ~10 500 because `enforce_policy` fired automatic checkpoints during the load.
- **Name the case, not the operation.** An arm called `overwrite` that rewrites each document with its own code is the empty-diff best case for a difference-emitting writer; reported as labelled, a 5x improvement would have read as 24x to 54x.
- A timing guard's scope is its enclosing block. A probe named for one expression, left to drop with the match arm, reported the whole arm -- first over-scoping the cost and then hiding the fix working.
- **A comment explaining why something is impossible is the shape that stops the next reader investigating.** A true premise does not license a "cannot": a writer's comment said clearing could not be narrowed because the old code was not known without reading it back, and reading it back was available the entire time, for four milestones.
- A hang is not evidence until the harness is ruled out. Three probes hung in one week for harness reasons while the subject matter supplied a plausible storage-layer explanation on demand.
