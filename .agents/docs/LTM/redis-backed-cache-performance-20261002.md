# Redis-backed dense cache comparison ( 2026-10-02 )

The initial comparison and causal A/B use yesno commit `7276ddf`, before the
windowed-read fix in `5a8da26`. Post-fix measurements are recorded at the end.

## Construction and boundaries

The scratch instrument is `.agents-workspace/tmp/flight-bitvector-bench/` at yesno
commit `7276ddf`. On this 20-core AArch64 host it creates a checkpointed key of
1,024 bitmap chunks, each 8,192 bytes of `0x55`, through `OrdSet::from_chunks`.
The half-dense 8 MiB bitvector is served by `yesno-flight` on loopback TCP as
`SetWire::Bitvector`. The bare-key request uses the contiguous `dense_span` borrow
path. `bare_gather` additionally copies decoded Arrow values into one `Vec`, matching
the contiguous output of Redis `GET`. A second ticket selects 64 chunks in the
middle of the same key, giving a 512 KiB range; Redis uses `GETRANGE` over the
corresponding bytes. Each read arm warms three times. The controlled runs use
100 timed requests and alternate the order of Redis and Flight full-entry reads.

Redis 7.4.10 ran from the cached `redis:7-alpine` image on host-network loopback,
bound only to `127.0.0.1`, with `save ""`, `appendonly yes`, `appendfsync always`,
and `auto-aof-rewrite-percentage 0`. The Rust Redis client sends RESP2 over one
persistent TCP connection with `TCP_NODELAY=true` and reads each full reply into a
new `Vec`. `CONFIG GET` verified these settings. Redis `MEMORY USAGE cache` was
10,485,808 bytes for the 8,388,608-byte value. Flight uses one persistent Tonic
channel and decodes all Arrow batches through end-of-stream. The benchmark checks
byte boundaries on both read paths. Timings include client decoding and copying;
they exclude connection setup and key creation. The synthetic repeated byte is
deliberately half-dense but is more compressible than real quantization codes.

This host has two CPU types: Cortex-A725 at 2.808 GHz and Cortex-X925 at 3.9 GHz,
with load average around 27 during these trials. To control placement, Redis was
pinned to X925 CPU 15 and the benchmark process ( including Flight server and
client ) to X925 CPUs 16 and 17. The accepted Flight socket had
`TCP_NODELAY=true` for the controlled runs. Raw samples and phase timings are in
`.agents-workspace/tmp/redis-compare-pinned-{1,2}.log`.

| Paired read, 100 requests per trial | Redis median / p95 / p99 | yesno Flight median / p95 / p99 |
| --- | ---: | ---: |
| Full 8 MiB, trial 1 | `GET`: 4.197 / 4.778 / 4.955 ms | gather: 3.594 / 4.327 / 14.779 ms |
| Full 8 MiB, trial 2 | `GET`: 4.090 / 4.727 / 4.846 ms | gather: 3.622 / 4.277 / 15.301 ms |
| Middle 512 KiB, trial 1 | `GETRANGE`: 0.198 / 0.315 / 0.371 ms | gather: 0.586 / 0.916 / 1.005 ms |
| Middle 512 KiB, trial 2 | `GETRANGE`: 0.205 / 0.313 / 0.331 ms | gather: 0.614 / 0.892 / 1.091 ms |

On pinned fast cores, yesno Flight's 8 MiB median is 11-14% lower than Redis
`GET`, even after gathering Arrow values into one contiguous buffer. Redis is
about three times faster on the 512 KiB range. Redis has tighter tail latency:
its full-entry p99 stays below 5 ms, against about 15 ms for the paired Flight
requests. The earlier unpaired Flight arms in these same pinned trials had
sporadic 200-420 ms outliers, so their p99 was much worse; this transport tail
cannot be ignored merely because the paired arms did not hit it. Local yesno
borrow-batch construction is only 0.178-0.180 ms median, well below its Flight
latency. Gather and borrow medians were nearly equal in the unpaired pinned
arms ( about 2.8-3.0 ms ), so the final contiguous copy is not the main cost.

Unpinned results were sensitive to placement and load. In one 80-request run,
Redis `GET` had a 4.395 ms median and Flight borrow 6.575 ms; a later alternating
80-request run had Redis at 5.017 ms and Flight gather at 5.219 ms. Those logs
are `.agents-workspace/tmp/redis-compare-true.log` and
`.agents-workspace/tmp/redis-compare-paired.log`. With the custom Flight incoming
socket's default `TCP_NODELAY=false`, the 512 KiB response again had a roughly
40 ms delayed tail ( p95 41.544 ms ) in
`.agents-workspace/tmp/redis-compare-false.log`. Enabling NODELAY removes that
particular mode but has exposed 200 ms retransmission-scale tails, including in
the pinned runs. It is a diagnostic A/B, not a standalone production fix.

## Durable overwrite, separate API comparison

One pinned 8 MiB run also alternated 12 timed full-key overwrites after two warmups.
yesno used direct `Db::batch().store_set()` plus `commit()` and `wait_visible()`,
with the key already checkpointed and the WAL kept below automatic checkpoint
thresholds. Redis used the same loopback TCP connection and `SET` under
`appendfsync always`. No yesno checkpoint was included because the WAL sync is
the durability boundary. The observed yesno WAL grew to 118,588,736 bytes after
the initial entry and 14 overwrites. An earlier unpinned run reported Redis
`aof_current_size` of 142,606,971 bytes after its read and write comparisons,
with no delayed fsyncs.

| Write, 12 requests | Median | p95 | p99 |
| --- | ---: | ---: | ---: |
| yesno direct durable `store_set` | 14.798 ms | 17.957 ms | 17.957 ms |
| Redis TCP `SET`, AOF sync always | 19.273 ms | 20.082 ms | 20.082 ms |

The API boundary differs: yesno is called in process and Redis includes TCP and
RESP. An earlier unpinned run was closer ( 18.754 vs 19.610 ms median ), showing
the same CPU-placement sensitivity as reads. The numbers do not establish which
store has a faster *over-the-wire* durable write. This Flight instrument does
not exercise a bulk bitvector write endpoint. It also uses repeated overwrites
of one key rather than a multi-key cache workload or cold restart. The read
comparison is therefore the stronger result for the current packed-KV question.

Reproduction, using an unused loopback port and the same Redis configuration:

```sh
mkdir -p .agents-workspace/tmp/redis-compare-repro-data
docker run --rm -d --network host --cpuset-cpus=15 \
  -v "$PWD/.agents-workspace/tmp/redis-compare-repro-data:/data" \
  --name yesno-redis-compare \
  redis:7-alpine redis-server --bind 127.0.0.1 --port 6389 \
  --save '' --appendonly yes --appendfsync always \
  --auto-aof-rewrite-percentage 0
taskset -c 16,17 env CARGO_TARGET_DIR="$PWD/target" cargo run --release \
  --manifest-path .agents-workspace/tmp/flight-bitvector-bench/Cargo.toml \
  -- 1024 100 true 6389 0
docker stop yesno-redis-compare
```

The final argument is the number of timed writes; use `12` to include the
durable overwrite experiment. The first read argument gives chunk count; `1024`
means 8 MiB. The server socket argument is `true` or `false`.

## Cause of the 512 KiB range gap ( 2026-10-02 follow-up )

The `DoGet` worker in `yesno-flight/src/lib.rs` executes `snap.load(t.key)` before
it branches on `SetWire::Bitvector`. For a bare-key bitvector ticket, it therefore
materializes **all** chunks of a key even if `dense_span` later lends the requested
window and the loaded set is never used. Redis `GETRANGE` reads only the named
byte slice. The full load is the principal source of the threefold range gap.

The scratch instrument now stores a second key containing only the same middle
64 chunks as the 1,024-chunk key. Both tickets request exactly prefixes
`512..576`; each returns the same 512 KiB and uses the same borrowed path. The
same Redis key and `GETRANGE` are the reference. The benchmark process stayed
on X925 CPUs 16-17 and Redis on X925 CPU 15, with the Flight socket NODELAY
enabled. Three independent runs of 100-150 requests each are in
`.agents-workspace/tmp/redis-range-gap-{1,2,span}.log`.

| Timed operation | Run 1 median | Run 2 median | Run 3 median |
| --- | ---: | ---: | ---: |
| Flight 512 KiB from 1,024-chunk key | 0.535 ms | 0.525 ms | 0.531 ms |
| Flight same 512 KiB from 64-chunk key | 0.273 ms | 0.269 ms | 0.265 ms |
| Redis `GETRANGE` same 512 KiB | 0.199 ms | 0.195 ms | 0.195 ms |
| Local `snap.load` of 1,024 chunks | 0.220 ms | 0.218 ms | 0.228 ms |
| Local `snap.load` of 64 chunks | 0.015 ms | 0.015 ms | 0.016 ms |

The large-key Flight penalty is 0.256-0.266 ms, or 78-79% of its 0.330-0.336 ms
gap to Redis. The local full-key load difference is 0.203-0.212 ms. The
separate `dense_span` operation over the requested 64 chunks costs 0.013 ms
median for **both** keys in the third run, ruling out the borrowed bitmap layout
as the cause of the key-size penalty. Most of the Flight difference appears
before the response headers: the large and small median header times were
0.368 vs 0.139 ms in run 1, 0.433 vs 0.122 ms in run 2, and 0.391 vs
0.191 ms in run 3. The worker loads the set before producing its first batch;
Tonic's header delivery in this benchmark tracks that delay. Directly comparing
the small-key Flight path with and without a
client-side contiguous gather gave 0.237 vs 0.244 ms in run 2 and 0.236 vs
0.250 ms in run 3, so that copy costs about 7-14 us, far below the gap.

The residual small-key gap to Redis is roughly 0.07 ms in the interleaved
same-range comparison. It includes Tonic request setup, the blocking worker,
Arrow framing and decoding, the small-key load, and the final copy; these were
not individually isolated. A targeted optimization is to attempt the
bare-key `dense_span` loop before loading the full set, and load the set only
if borrowing stops and the staging fallback needs it. Filtered tickets must
continue through their expression result: borrowing `t.key` for a filter
would silently return excluded bits. A change also needs a decision on whether
corruption in chunks outside a requested window should still fail a range read,
since the current eager load may detect it. No production code was changed in
this follow-up.

The existing `Snapshot::key_stream_prefix_range` is a second option for the
bare-key staging fallback: it restricts the index scan to the ticket's prefix
window and implements `ChunkStream`, which `MaskStream::dense` accepts. That
could avoid the full-key load even when borrowing stops. Its streaming error
and snapshot-lifetime behavior would need to be preserved in a change.

### Landed fix

Commit `5a8da26` moved the bitvector arm before the whole-key load and made
bare-key reads use a prefix-restricted stream on the staging and other set-wire
paths. The implementing agent measured the large-key minus small-key penalty
for the same 512 KiB window across three 150-request runs: bitvector
`+0.209, +0.225, +0.252 ms` before, then `+0.005, -0.029, -0.002 ms` after;
container wire `+0.274, +0.281, +0.319 ms` before, then
`+0.007, -0.033, +0.001 ms` after. Tests pin that a filtered ticket still
returns the expression and that corruption outside the requested window does
not fail the windowed read while a whole-key read still fails. Both `gate.sh`
and `gate-pg.sh` passed for that commit. The Redis numbers above remain a
pre-fix baseline.

### Post-fix Redis remeasurement

The same scratch crate was rebuilt against yesno `5a8da26` and Redis 7.4.10.
Redis remained pinned to Cortex-X925 CPU 15 and the benchmark process ( Flight
server and client ) to CPUs 16-17. The database again held one 1,024-chunk,
8 MiB half-dense key and a second 64-chunk key containing the identical middle
512 KiB. The client requested that middle window on both keys and Redis used
`GETRANGE` over the corresponding bytes. Each run warmed all arms and timed
150 requests per arm. The range phase rotates the order of large-key Flight,
small-key Flight, and Redis across rounds; the earlier two exploratory
post-fix logs kept Redis last and showed a variable 0.069-0.199 ms Redis median,
so the rotated runs are the comparison to use. Raw per-request samples are in
`.agents-workspace/tmp/redis-range-postfix-rotated-{1,2,3}.log`.

| 512 KiB read, `TCP_NODELAY=true` | Large yesno key median | Small yesno key median | Redis median |
| --- | ---: | ---: | ---: |
| Rotated run 1 | 0.238 ms | 0.246 ms | 0.200 ms |
| Rotated run 2 | 0.227 ms | 0.229 ms | 0.193 ms |
| Rotated run 3 | 0.227 ms | 0.229 ms | 0.204 ms |

The old 0.256-0.266 ms large-key penalty is absent: the paired difference is
`-0.008, -0.002, -0.002 ms` across the three runs. Redis remains 0.023-0.038 ms
faster at the median on this small range, about 10-17% of the yesno time. That
residual includes the different protocol and Arrow decoding; this run does not
isolate its components. For a paired full 8 MiB read, yesno Flight's contiguous
gather took 3.173-3.270 ms median against Redis `GET` at 3.938-4.042 ms median.

The accepted Flight socket setting still matters independently of the window
fix. One additional 150-request run with its custom-incoming default
`TCP_NODELAY=false` had a 0.591 ms median and 41.439 ms p95 for the large-key
range, versus Redis at 0.235 ms median and 0.465 ms p95. Twenty of 150 large-key
Flight reads and 15 of 150 small-key reads took at least 35 ms; Redis had zero.
The raw log is `.agents-workspace/tmp/redis-range-postfix-default-socket.log`.
The fix removed wasted full-key work but did not change the socket's delayed
tail. With NODELAY enabled, one of the three full-entry runs still had a
210 ms maximum, so changing only the socket setting is not established as a
complete tail-latency fix.
