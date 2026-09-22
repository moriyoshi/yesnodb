# GPU Offload for Long Dense Sets on Unified Memory

Exploration dated 2026-09-23 on the project's own development host, an NVIDIA
**GB10** ( Grace-Blackwell, compute capability 12.1, 20 Arm cores -- 10
Cortex-X925 plus 10 Cortex-A725 -- and 121 GiB of memory shared between CPU and
GPU ). **No code was written into the crate and none should be** until the
conditions below are met; this document is the deliverable.

## Summary

The usual reason GPU offload fails for set algebra does not apply on this
machine, and a different reason takes its place. Single boolean operations are
worth **1.45x** over a fully threaded CPU -- not worth a CUDA dependency.
**Batched** operations, where one resident set is used against many filters,
are worth **9-11x** over the same threaded CPU and **40-83x** over the single
thread yesno actually uses today. The entire opportunity lives in the second
shape, and yesno already has an API with exactly that signature.

## Key Facts

- **There is no transfer to amortize.** GB10 reports `Addressing Mode: ATS` and
  `GPU C2C Mode: Enabled`: CPU and GPU are cache-coherent over NVLink-C2C with
  one physical memory and one NUMA node. `nvidia-smi` reports no separate
  device memory at all. The PCIe copy that normally makes bitmap offload a
  guaranteed loss does not exist here.
- **A CUDA kernel dereferences plain `malloc`'d memory directly**, verified
  against a CPU reference count. yesno's bitmap payloads live in ordinary
  `arrow-buffer` allocations, so offload needs **no copy, no pinning, no
  registration and no allocator change** -- which is what makes this worth
  writing down at all, given ARCHITECTURE's `arrow-buffer` containment policy.
- **One memory means one bandwidth ceiling.** Measured AND-cardinality over two
  contiguous dense bitmaps: a single core saturates at **31 GB/s**, all twenty
  cores at **102 GB/s**, the GPU at **169 GB/s**. The GPU's advantage over the
  whole CPU is therefore ~1.65x and **cannot** be more for any operation that
  touches each byte once. Nothing about a GPU changes a bandwidth-bound problem
  when the memory is shared.
- **Kernel launch is 5.10 us** ( median, empty kernel, launch plus sync ). That
  is the floor under every offload decision and rules out per-container work.
- **Per-container launches are fatal, batching is free.** Over 65 536 separate
  8 KiB containers -- yesno's real layout -- one launch per container costs
  **269 ms** against **6.6 ms** for a single launch over a pointer table, 41x
  worse. But the scattered layout itself costs the GPU almost nothing:
  **162 GB/s** scattered against 169 contiguous, about 4%. The obstacle is
  launch granularity, not the data structure.
- **Reuse is the whole opportunity.** One 32 MiB set against K filters, GPU
  against all twenty cores: **6.7x** at K=1, **8.8x** at K=4, **9.2x** at
  K=16, **11.1x** at K=64, with GPU throughput rising 17.6 -> 192.5 Gop/s.
  Against the single thread yesno uses today, 40-83x.
- **`OrdSet::view_intersection_cardinalities_batch` is already this shape** --
  one resident view against a slice of filters -- and Flight's server calls it.
  If any offload is ever built, that is its call site.

## The measurement that was wrong first, and why

**The first batched kernel measured 1.06-1.23x and would have closed this
question as "no opportunity".** It indexed filters on `grid.y` and chunks on
`grid.x`. CUDA schedules x-major, so the blocks sharing a chunk are spread
across the whole grid and each chunk is re-read from DRAM once per filter: the
kernel was measuring the bandwidth wall it existed to escape. Its throughput
gave it away -- flat at ~20 Gop/s for every K, which is the *single-operation*
bandwidth figure.

Worse, the comparison was not symmetric. The CPU arm nests
`for chunk { for filter }`, so the chunk stays in L1 across all K and the CPU
*was* exploiting the reuse the GPU was not. The measured ratio fell from 2.50
to 1.06 as K rose, which reads exactly like "the GPU does not benefit from
batching" and was in fact "my kernel does not, and the CPU does".

Restructuring to one block per chunk, holding the chunk in registers and
looping over filters inside the block, moved the answer by about **9x**. The
rule this produces: **when a GPU arm shows flat throughput as the work per byte
rises, the kernel is not exploiting reuse -- check the schedule before
believing the conclusion**, and check that both arms exploit it or neither
does.

## Details

### The three regimes, measured

AND-cardinality, GB10, CUDA 13.0, all figures medians of five repetitions with
the GPU result verified against a CPU reference every repetition.

Contiguous operands, each byte touched once:

```text
  operand     cpu 1 thread   cpu 20 threads        GPU
    8 KiB       10.9 GB/s              n/a    2.3 GB/s
  128 KiB       23.9 GB/s              n/a   27.4 GB/s
    8 MiB       28.0 GB/s        42.9 GB/s  156.5 GB/s
  128 MiB       30.8 GB/s        97.5 GB/s  161.6 GB/s
  512 MiB       31.2 GB/s       102.2 GB/s  168.7 GB/s
```

Below ~8 MiB the 20-thread column is an artifact: OpenMP forks a team per call
at 300-500 us. A persistent pool would remove that, so read only the two
largest rows as the real CPU ceiling.

Scattered 8 KiB containers with one batched launch, which is yesno's layout:

```text
  chunks   set size   cpu 1 thread   cpu 20 threads        GPU
      16   0.12 MiB       16.2 us         225.0 us      6.6 us
    4096     32 MiB      3111.6 us        1062.9 us    418.8 us
   65536    512 MiB     47554.5 us        9780.9 us   6607.9 us
```

One resident 32 MiB set against K filters, GPU against 20 cores:

```text
  K=1   6.71x      K=4   8.80x      K=16   9.22x      K=64  11.08x
```

### Why single operations cannot be worth it

An AND-cardinality reads 16 bytes per `AND` + `popcount` + accumulate. That is
about an eighth of an operation per byte, so every processor sharing this
memory is bandwidth-bound above cache and the ranking is fixed by how much of
one memory system each can reach. The GPU reaches 1.65x what twenty cores do.
Against that: CUDA is a hard dependency on one vendor's hardware, absent from
every deployment target this project names, and core holds a five-direct-
dependency budget that a JIT already had to be kept out of. **1.65x does not
buy that, and the same factor is available by threading the CPU**, which yesno
does not do yet and which costs no dependency, no hardware assumption and no
portability.

### Why batching can be

Reuse is what lifts arithmetic intensity off the bandwidth floor. With one set
held in registers and K filters streamed past it, DRAM traffic is
`O( data + filters )` rather than `O( data * filters )`, and the GPU's
throughput rises with K where the bandwidth-bound case is flat. At K=16 the
filters ( 128 KiB ) sit in L2 and the data is read once; measured GPU
throughput reaches 192.5 Gop/s, about 11x twenty cores and 83x one.

### Conditions before any of this becomes code

1. **A satellite crate, never core.** The `yesno-jit` precedent applies exactly:
   an optional crate behind a feature, keeping core's dependency budget and its
   portability. Core must build and pass its gate with no CUDA anywhere.
2. **Batched call sites only.** `view_intersection_cardinalities_batch` and
   nothing else, because it is the only shape measured to pay.
3. **An admission floor, measured like `AUTO_MIN_CHUNKS` was.** The 5.10 us
   launch and the small-set rows put the crossover against a *threaded* CPU
   somewhere above a few MiB; it has not been located precisely and must be
   before anything dispatches automatically.
4. **A materializing result is unmeasured.** Every figure here returns counts --
   a few bytes. `and_into` writes a third stream and changes the accounting;
   do not extrapolate.
5. **The CPU baseline must be the threaded one.** Comparing against yesno's
   current single thread would claim 40-83x for something a thread pool
   supplies most of.

### The discrete case, measured on the other machine

The project's x86 reference machine -- the Intel i9-9880H MacBook Pro -- has
**two** GPUs, and measuring both turns the GB10 result from a fact into a
principle. OpenCL probe, 128 MiB per operand, counts verified against the CPU:

```text
  CPU  1 thread                     19.6 GB/s
  CPU 16 threads                    29.1 GB/s

  Intel UHD 630 ( integrated, shares system DRAM )
    upload                           3.3 GB/s
    resident kernel                 16.8 GB/s
    break-even                      NEVER

  Radeon Pro 5500M ( discrete, PCIe x16, 8 GiB VRAM )
    upload                           5.3 GB/s
    resident kernel            108-137 GB/s
    one-shot vs 16 threads           0.17x   ( a 6x LOSS )
    break-even vs 16 threads         7.5 operations on resident data
```

**The integrated GPU is a flat no.** It shares the CPU's memory, so it has no
bandwidth to offer, and its resident kernel is *slower than a single CPU core*
( 16.8 against 19.6 GB/s ). Shared memory removes the transfer problem without
supplying the thing that would make offload worth doing.

**The discrete GPU is conditionally yes, and the condition is residency.** Its
VRAM is genuinely fast -- 108-137 GB/s, about 4x what all sixteen CPU threads
reach -- but a one-shot offload of host-resident data loses by **6x**, because
the upload alone costs ten times the kernel. It only pays if a set is **pinned
in VRAM and queried at least eight times**, and the 8 GiB cap bounds how long
"extremely long" may be.

**The contrast is the durable part.** Same operation, same fixture, three
processors:

| | transfer | resident speed | verdict |
|---|---|---|---|
| GB10 unified | none | 1.65x CPU | offload is a pointer; the win is reuse, not residency |
| Radeon discrete | 6x the kernel | 3.7x CPU | offload is a data-residency project |
| UHD integrated | none | 0.86x CPU | no |

Memory architecture, not GPU speed, decides which problem you are solving. On
GB10 there is nothing to manage and the question is purely "is there reuse". On
a discrete card the question is "can a set live on the device across many
queries", which is a lifecycle and eviction design -- a far larger commitment
than the 3.7x ceiling justifies for a laptop that is not a deployment target.

**One measurement here is mine, not the hardware's.** The probe's "pinned"
upload path reported 2.0 GB/s, *worse* than pageable. That is a bad
implementation -- `CL_MEM_ALLOC_HOST_PTR` plus map plus `memcpy` adds a host
copy rather than removing one -- and not evidence that pinned DMA fails to
help. The pageable 5.3 GB/s is also well under PCIe 3.0 x16's practical
ceiling, so the break-even of 7.5 should be read as an **upper bound** that a
proper zero-copy upload would lower, perhaps to 3-4. It does not change the
verdict, because the one-shot case loses by 6x regardless.

### Would a VRAM-resident LRU cache justify the transfer?

Asked directly, measured directly. **Yes above a hit rate of about 87%, with a
ceiling near 5x -- and the failure mode is severe enough that it is the wrong
tool on this hardware.**

With a *perfect* cache -- both operands already in VRAM, nothing transferred --
the Radeon wins by 3.1x to 7.0x over sixteen CPU threads for operands of 32 MiB
and up ( 16 MiB loses at 0.44x: too little work for the dispatch ). That is the
ceiling any cache policy is competing for.

The economics are set by one ratio. Upload runs at ~3.9-5.3 GB/s while sixteen
CPU threads compute the whole operation at ~24 GB/s effective, so **uploading
one operand costs about three times what the entire CPU operation costs**.
Measured at 512 MiB: a cache hit costs 0.20 CPU-operations, a miss costs
**3.3**. Break-even is therefore

```text
  h * 0.20 + ( 1 - h ) * 3.3 < 1   =>   h > 0.74
```

A cache that hits 74% of the time breaks even; a perfect one is worth 4.9x.

**Corrected 2026-09-23 from an earlier 87% in this document.** That figure used
the contaminated 25 548 us kernel time and charged a miss for uploading *both*
operands rather than the one that missed. The clean sweep and the single-operand
miss give 74%. The verdict does not move -- the one-shot case still loses by 6x
-- but the number was wrong and was quoted.

**Roaring semantics work against the idea, and this is the part that is easy to
miss.** A per-query sweep with the large set cached and only the filter
uploaded lost to the CPU at *every* filter size, down to D/64. The reason is
not the transfer rate: in a sparse set library a small query against a large
set does work proportional to the **query**, because only intersecting chunks
are touched. So caching the big operand saves proportionally less as the query
gets smaller, while the query still has to be uploaded. The cache is squeezed
from both ends -- small queries do too little work to amortize their own
upload, and large queries have to upload a large operand anyway.

The regime that survives is the one where repeated queries touch **most of a
large resident set**, and both sides stay resident: fixed filters against a
fixed view, which is
`view_intersection_cardinalities_batch` again. That is the same conclusion the
GB10 measurements reached from the other direction, and it is worth stating as
one rule: **the win is reuse, and a cache is only a way of manufacturing reuse
when the hardware will not give it to you for free.**

One factor is favourable and should be recorded. yesno's containers are
immutable and refcounted per snapshot, so a cache keyed on container identity
cannot go stale -- there is no invalidation problem, which is normally the
hardest part of such a design.

Against that: 87% is a demanding hit rate for a 8 GiB cache over a working set
of unknown size; a miss costs 6.3 CPU-operations, so the policy's bad case is
six times worse than not having it; and the whole apparatus buys at most 5x on
a laptop that is not a deployment target. **On GB10 the same workload gets
9-11x with no cache, no policy and no failure mode**, because there is nothing
to transfer. The cache is a workaround for an architecture, and the
architecture that needs it is the one the project does not deploy on.

### Projected to cloud GPU instances

**Everything in this section is projection from published specifications, not
measurement.** Only the GB10 and the Radeon rows in this document were
measured. What transfers between machines is the *structure* -- the cost model
and the ratio that drives it -- not the constants.

The model, per AND-cardinality over two operands of D bytes, with `G` the GPU's
resident bandwidth, `U` the host-to-device bandwidth and `C` the host CPU's
achievable all-core bandwidth, all in GB/s, costs expressed in units of one
whole CPU operation:

```text
  hit  ( both operands resident )   = C / G
  miss ( one operand uploaded )     = C / ( 2U ) + C / G
  break-even hit rate  h* > 1 - ( 1 - C/G ) / ( C / 2U )
  ceiling at h = 1                  = G / C
```

Applying it with published bandwidths and *estimated* host-CPU figures:

```text
  accelerator      G      U      C     hit    miss     h*    ceiling
  Radeon 5500M    118    3.9   24.0   0.203   3.28    74%      4.9x   [MEASURED]
  T4    g4dn      320     13    100   0.312   4.16    82%      3.2x
  A10G  g5        600     25    100   0.167   2.17    58%      6.0x
  L4    g6 / G2   300     25    150   0.500   3.50    83%      2.0x
  V100  p3        900     13    120   0.133   4.75    81%      7.5x
  A100  p4de/A2  1935     25    200   0.103   4.10    78%      9.7x
  H100  p5 / A3  3350     50    400   0.119   4.12    78%      8.4x
  H200  p5e      4800     50    400   0.083   4.08    77%     12.0x
```

**The striking thing is how invariant `h*` is: 77-83% across four GPU
generations**, and 74% on the measured laptop. PCIe bandwidth and host DRAM
bandwidth have scaled together, so `C / 2U` stays near 4 whatever the year. The
*ceiling* improves a great deal -- 3.2x to 12x -- but the hit rate you must
achieve to collect any of it barely moves. A10G is the outlier at 58% only
because `g5` pairs a fast GPU with a comparatively weak host CPU.

`C` is the least certain input, and `h*` is sensitive to it: halving `C` halves
`C/2U` and raises `h*`. Treat the table as the shape of the answer and measure
`C` on the chosen instance before committing.

### In the cloud, price-performance is the real argument, not latency

Aggregate memory bandwidth per dollar-hour, on-demand list prices:

```text
  c6i.32xlarge  128 vCPU        200 GB/s   $ 5.44     37 GB/s per $/hr
  g6.xlarge     1x L4           300 GB/s   $ 0.81    373
  g5.xlarge     1x A10G         600 GB/s   $ 1.01    596
  p4d.24xlarge  8x A100      12 440 GB/s   $32.77    380
  p5.48xlarge   8x H100      26 800 GB/s   $98.32    273
```

A GPU instance offers **7-16x more bitmap bandwidth per dollar** than a CPU
instance, and the cheapest single-GPU shapes are the best of them, because they
pair one accelerator with a small host -- which is exactly the shape this
workload wants, the CPU only orchestrating. **This is a stronger argument than
any latency ratio in this document**, and it is the one that should drive the
decision.

It is also entirely conditional on residency. At `h = 0` the same instance is
about 4x *slower* than the CPU and costs more, so the price-performance case
and the correctness of the residency design are the same question.

### The miss policy matters more than the hit rate

**Corrected 2026-09-23 after the maintainer pushed back that 74% is not a
demanding hit rate. They were right, and the framing was wrong for a deeper
reason than the number.**

Every break-even figure above assumes **fill-on-miss**: a miss pays the upload
*and* the kernel, costing about four CPU-operations. That is one policy, and
not the one this codebase already uses elsewhere. The JIT's `try_cardinality`
returns `None` on an unsupported shape and the scalar evaluator runs. Applying
the same shape here -- **on a miss, compute on the CPU and fill off the
critical path** -- makes a miss cost exactly one CPU-operation, and the
economics change completely:

```text
  A100, speedup by hit rate     50%    74%    90%    95%    99%   100%
  fill on miss                 0.48x  0.88x  1.99x  3.30x  6.99x  9.71x
  CPU fallback on miss         1.81x  2.97x  5.19x  6.76x  8.93x  9.71x
```

**Under CPU fallback there is no break-even threshold at all.** Any hit rate is
a win, because the worst case is the path that would have run anyway. A 74%
hit rate is worth about 3x on an A100 rather than nothing.

The tail matters at least as much as the mean, and it is what really condemns
fill-on-miss. At a 95% hit rate that policy averages 3.30x while **5% of
queries run 4.1x slower than the CPU** -- a p95 regression that is usually
disqualifying for a query engine whatever the mean says. Under CPU fallback the
slowest query equals the CPU and there is no tail regression to explain.

Two things survive the correction. The payoff is still dominated by the *miss*
rate rather than the hit rate -- 74% to 90% to 99% is 3.0x, 5.2x, 8.9x -- so
most of the value is in the last few percent and sizing still beats cleverness.
And fills are not free, only off the critical path: they consume host and PCIe
bandwidth that competes with the fallback doing real work, so the fill rate
bounds how fast the hit rate can climb, and a working set churning faster than
fills converge would never reach a good one. Rate-limited fills and a size
admission rule are the controls, the same shape as `worth_jitting`.

### The strategic consequence: capacity, not eviction

An LRU cache is what you build when the working set does not fit. In a cloud
the capacity is a purchase decision: 24 GiB on an A10G or L4, 80 GiB on an
A100, 141 GiB on an H200, and 8 of them on a `p4d` or `p5`. A dense bitmap
holds 8 ordinals per byte, so 24 GiB is roughly 200 billion dense ordinals and
80 GiB is 670 billion.

**If the working set fits, `h = 1` by construction and there is no cache, no
policy and no bad case** -- it is GPU-resident storage with an admission rule,
which is a far smaller and more predictable thing to build than an eviction
policy whose miss costs four CPU-operations. yesno's immutable refcounted
containers make the residency mapping stable, which is the property that makes
this tractable at all. The design question becomes *sizing*, and the failure
mode becomes *refusing* an oversized working set rather than thrashing on it.

### The unified-memory instances, and a caveat about them

GB200 and GH200 shapes exist in all three clouds ( AWS `P6e-GB200`, GCP `A4X`,
Azure `ND GB200 v6` ), and on those the GB10 measurements in this document
apply directly: no transfer, no cache, no hit rate.

**But "no transfer" buys parity, not a win, for single operations.** Coherent
GPU access to host memory crosses NVLink-C2C at roughly 450 GB/s each way,
which is comparable to the Grace CPU's own memory bandwidth -- which is
precisely why the measured GB10 figure is only **1.65x** and not the 10x its
HBM would suggest. Reaching HBM bandwidth means migrating pages into HBM, which
is a transfer again, merely a much faster one. On these machines the win still
comes from **reuse** ( the measured 9-11x batched result ), not from the
absence of a bus.

### Adaptive admission, and the instrument to build first

The residency decision cannot be made statically -- nothing in a set's shape
says whether it will be queried again -- so it has to be observed. The
threshold that falls out is unusually clean: a fill costs `D/U` of background
bandwidth and each subsequent hit saves `( 1 - C/G )` of a CPU operation, and
**both scale linearly in `D`, so the break-even is independent of set size**:

```text
  A10G   admit after 2.4 queries
  A100   admit after 4.5 queries
  H100   admit after 4.5 queries
```

The admission rule is therefore a **counter, not a cost model** -- "admit after
about five observed accesses" -- and it lands on the same number as the
Radeon's independently measured 4.5-7.5 break-even.

Adaptivity is safe here **because of the fallback policy, not despite it**. A
wrong admission wastes background bandwidth; a wrong eviction costs one CPU
operation. Neither is catastrophic, so the detector may be crude and still be
correct. Under fill-on-miss a misprediction cost four CPU-operations and the
same adaptivity would have been dangerous. The policy choice is what licenses
the heuristic.

The fill is also the moment to fix the layout: gathering the scattered 8 KiB
container payloads into one contiguous VRAM buffer removes the pointer-table
indirection and the measured 4% scatter penalty, and simplifies the kernel. The
copy is happening regardless.

**The first thing to build is not GPU code.** An access-frequency observer on
the ordinary CPU path -- a counter keyed on container identity, with decay --
costs no dependency and no hardware, and it produces the one quantity none of
the projection above can supply: **the hit rate a real query stream achieves
against a given VRAM budget.**

It also closes an unrelated open item. `jit-auto-min-chunks-rests-on-a-
withdrawn-number` in TODO.md is blocked on "evidence of eligible shape
recurrence on the same worker before cache saturation", which is the identical
distribution. **One instrument answers both**, and if recurrence turns out flat
the answer is that neither the GPU satellite nor a lower JIT threshold is worth
starting -- learned for the price of a counter rather than a CUDA crate.

### The observer was built, and it corrected this section

*Added 2026-09-23.* The instrument the previous section asks for now exists at
`.agents-workspace/tmp/hotspot-observer/` -- a standalone research crate, 30
tests, clippy-clean, not shipped. It keys a reuse-distance histogram and an
admission simulator on a hashed key, and two extractors feed it: `( set,
prefix )` container identity for this question, and a planned-shape fingerprint
for the JIT's. The full write-up is in `JOURNAL.md` under
*2026-09-23 -- The hotspot observer*.

It changed three things stated above.

**One: the threshold is not free.** This section argued adaptivity is safe
because a wrong admission only wastes background bandwidth. That is true of
admission and false of the threshold. Where capacity binds -- 1 MiB against a
64 MiB corpus -- raising `admit` from 1 to 10 *halves* the hit rate, because the
hot keys are delayed into a cache that evicts them before they qualify. The
counter pays when the cache can hold a working set and costs when it cannot.

**Two: the threshold and the decay are one parameter.** At 16 MiB and Zipf 1.2,
sweeping both:

```text
  half-life    admit>=1        admit>=3        admit>=5        admit>=10
  none      72.0% 2295 B   74.2%  693 B   75.1%  393 B   75.7%  183 B
  200 000   72.0% 2295 B   74.8%  490 B   75.9%  278 B   76.9%   91 B
   20 000   72.0% 2295 B   78.0%  161 B   78.2%   25 B   64.2%   13 B
    2 000   72.0% 2295 B   69.0%   16 B   50.5%   10 B   28.1%    7 B
```

( hit rate and fill bytes per operation. ) At half-life 20 000, `admit>=10`
falls *below* fill-on-miss, because evidence decays faster than ten
observations accumulate. So "admit after about five" is half a parameter. The
rule is **admit after about five observations within a window of roughly 15 000
accesses**, and the window must be quoted with the threshold.

**Three: the win is in bandwidth, not hit rate.** The best row -- half-life
20 000, `admit>=5` -- is 78.2% hit rate at 2.99x, with **25 bytes of fill
traffic per operation and 1% futile admissions**, against fill-on-miss's 72.0%,
2.58x, 2295 B/op and 65% futile. The hit rate moves six points; the fill
bandwidth falls **92-fold**. On a discrete accelerator behind PCIe that ratio,
not the hit rate, is what decides whether the link can carry the policy.

**The projection above survives an independent check.** At a 74.2% hit rate the
simulator reads 2.71x; the A100 row in the table above reads 2.97x at 74%, from
published specs and a bandwidth model with no simulator in it. Two unrelated
derivations within 10% of each other is worth more than either alone.

**What it still does not supply is a hit rate.** Every figure here is
conditional on a synthetic stream whose skew is a dial, and the program prints
that caveat itself. `--trace` replays a captured key stream through the same
counters. The remaining work is a capture point on the ordinary query path.

### CUDA and OpenCL are the same speed on this device

*Measured 2026-09-23.* The batched AND-popcount kernel, written the same way
in both APIs -- one work group per row, the row staged in local/shared memory,
one thread per filter -- run in one process against identical data, pinned,
interleaved, nine rounds, with a CPU reference checking both:

```text
  chunks  filters      cuda     opencl   opencl/cuda
     256       16    0.077ms    0.079ms        1.03x
     256       64    0.249ms    0.249ms        1.00x
     256      128    0.477ms    0.477ms        1.00x
    1024       64    0.944ms    0.938ms        0.99x
    1024      128    1.848ms    1.849ms        1.00x
    4096       16    0.960ms    0.971ms        1.01x
    4096       64    3.730ms    3.739ms        1.00x
    4096      128    7.393ms    7.502ms        1.01x
```

**Parity at every size and batch width**, and both agree with the CPU
reference on every one of the counts. The explanation is in the device string:
`NVIDIA GB10 / OpenCL 3.0 CUDA`. NVIDIA's OpenCL is layered on the same driver
and lowers to the same SASS, so there is no native-path advantage to spend.

**This is a stronger result than it looks, because neither arm is bandwidth
capped.** Effective throughput is 22-52 GB/s, far below what this device can
do -- the kernel runs 64 threads per block, which is two warps and poor
occupancy. If both arms were pinned at the memory ceiling, parity would be
trivial and would say nothing about the generated code. They are not, so it
does.

It also means **the kernel as written has headroom**, and whichever backend
ships should sweep block size before anyone quotes a speedup from it.

**Setup cost is not the differentiator either.** Steady state is CUDA 262 ms
against OpenCL 220 ms for context, allocation, upload and first build. The
very first run of the day measured **4899 ms for CUDA**, which is cold driver
load and not a property of the API -- a single measurement would have reported
an eight-fold gap that does not exist.

**What this changes.** The case for choosing CUDA first rested partly on it
being the native path on the only machine with a measurement. That reasoning
is now retired: on NVIDIA the API choice is performance-neutral, so it should
be decided on reach and ecosystem instead -- and an OpenCL backend that costs
nothing here also runs on AMD and Intel, where a CUDA one does not. The
remaining arguments for CUDA are ecosystem ones ( `cudarc` is a well
maintained crate with runtime loading and no build-time toolchain ), not
speed ones.

**An operational note that cost time.** Back-to-back runs of the harness hit
`cudaMalloc` out-of-memory on a machine with 111 GiB free, until the harness
released its device allocations and contexts explicitly instead of leaving
them to process exit. The failures were intermittent and skipped whole
configurations, which in a benchmark is worse than being slow: the skipped
rows look like the ones that did not fit.

### Reproduction

The GB10 probes are at `.agents-workspace/tmp/gpu-probe/` ( `ats.cu` for the
malloc-addressability verdict, `bw.cu` for the contiguous ceiling and launch
latency, `scattered.cu` for the container layout, `batch.cu` for reuse ), built
with `nvcc -O3 -arch=sm_121 -Xcompiler "-O3 -fopenmp -march=native"`. They are
research and do not ship; this document is what survives their deletion. The
fixture is the LCG seeded `0x2545_f491_4f6c_dd1d` that Stage 7 uses, so the
densities match the rest of the project's bitmap measurements. The CUDA-versus-OpenCL harness is
`.agents-workspace/tmp/cuda-vs-opencl/vs.cu`, one binary running both paths so
the data and the timing method cannot differ between them, built with
`nvcc -O3 -arch=sm_121 -o vs vs.cu -l:libOpenCL.so.1`. It vendors a minimal
`clmin.h` because the machine has `libOpenCL.so.1` and NVIDIA's ICD but no CL
headers; that is safe only because both arms are checked against a CPU
reference, so an ABI mistake would surface as wrong answers rather than as a
wrong timing. The Mac probe is
`.agents-workspace/tmp/gpu-probe-mac/mac.c`, built with
`clang -O3 -march=native -Wno-deprecated-declarations mac.c -framework OpenCL`;
OpenCL is deprecated on macOS and still functional, and was chosen because this
is a probe rather than a product.
