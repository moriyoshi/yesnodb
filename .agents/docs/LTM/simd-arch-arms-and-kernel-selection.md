# SIMD Arch Arms and Kernel Selection

## Summary

`ops::array`, `ops::bitmap`, `ops::mixed` and `ops::run` each carry a NEON arm and an x86_64 arm, reaching 910 tests on both architectures. Getting there produced two durable results beyond the code. First, **emulation is a correctness instrument and never a throughput one**: `qemu` inverted a SIMD verdict, predicting a regression where real silicon measured 8.5x. Second, **a technique's value is a property of the instruction set, not of the algorithm**: Harley-Seal, the canonical AVX2 popcount win, loses by 2.9x on NEON, because NEON has a cheap vector popcount and x86 has none.

## Key Facts

- **qemu-user translates each guest instruction serially**, so vector width buys nothing while the extra instruction count still costs. The same native aarch64 binary measures 1.62x for SIMD over scalar natively and 0.87x under `qemu-aarch64` -- no overlap between the sets.
- `x86_64-unknown-linux-musl` plus `rust-lld` cross-builds and runs under `qemu-x86_64` with **no system cross-gcc**. The `gnu` target fails at link ( the host's aarch64 gcc receiving `-m64` ).
- **The local gate is arch-blind.** On an aarch64 host `scripts/gate.sh` compiles `#[cfg( target_arch = "x86_64" )] mod simd` to nothing and goes green having never built it; CI's `ubuntu-latest` runner is what exercises it on real silicon.
- `_mm_cmpestrm` ( SSE4.2, **explicit**-length ) is the right array kernel: 1.09x-1.40x over the rotate ladder at every size, and structurally simpler because it returns the lane mask directly, which all four arms want.
- **AVX2 was measured and rejected for `ops::array`**: sixteen lanes needs a cross-lane permute per rotation, and the wider register buys exactly what the permute costs. It is the right choice for `ops::bitmap`, where the popcount must be synthesized anyway.
- x86 has **no unsigned 16-bit compare**; `cmpgt_epi16` is signed and mis-orders everything at or above `0x8000`, half the chunk space. `min_epu16( lo, hi ) == lo` recovers it.
- `madd_epi16` reads its inputs **signed** while bound B6 permits `d16` lanes up to 65535. Unpacking against zero costs one instruction and is correct across B6's whole range. **No test would have caught this**, because the corpus does not reach the affected range by accident.
- ANDNOT operand order reverses across the arches: `vbicq_u8( x, y )` is `x & !y`, `_mm256_andnot_si256( a, b )` is `!a & b`.
- **A kernel speedup is an upper bound on what the operation around it can gain.** The isolated AVX2 popcount win of 2.70x becomes 1.17x at the container-path level for `array x bitmap`.
- **Benchmarks need the same sabotage discipline as tests, plus a stated spread.** Disable the kernel, confirm the benchmark moves, confirm it moves for that kernel alone, and repeat until the spread supports the claim.

## Details

### Emulation establishes correctness, never throughput

There was no x86_64 hardware available when the first x86 arm was written, so the question was whether `qemu-x86_64` could stand in. It cannot, and `qemu-aarch64` proves it without needing an x86 machine at all -- one native binary, run both ways:

```text
                              scalar/simd ratio
  native aarch64              1.62x  1.61x  1.70x   ( SIMD wins )
  same binary, qemu-aarch64   0.91x  0.88x  0.87x   ( SIMD loses )
```

A qemu benchmark would have reported the SSSE3 array arm as a regression. It shipped with **no performance number at all**, deliberately, and correctness was established instead: 904 tests passing on x86_64 under emulation, the baseline taken before the arm landed so the comparison meant something, with each arm sabotaged in turn ( a dropped rotation reddens 3 array tests; `is_disjoint` and `contains_all` forced true redden one each ).

When real silicon arrived -- an Intel i9-9880H -- the prediction was confirmed on the target architecture directly: the arm is up to **8.51x** faster than scalar, and the same 906 tests pass natively that passed under emulation. Expect roughly 80 s under emulation against 3 s native; that ratio *is* the overhead, and it is why the timings cannot be trusted.

The reproducible command, recorded because the instrument does not ship:

```sh
rustup target add x86_64-unknown-linux-musl
LLD=$(find "$(rustc --print sysroot)" -name rust-lld -type f | head -1)
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$LLD" \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUNNER="qemu-x86_64" \
RUSTFLAGS="-C link-self-contained=yes -C linker-flavor=ld.lld" \
cargo test -p yesno-core --target x86_64-unknown-linux-musl --lib
```

`musl` rather than `gnu` is load-bearing: it is self-contained in the Rust toolchain, so no system cross-gcc is needed. The qemu run is not redundant with CI -- it is the only way to get a signal on the x86 arms *before* pushing, since the local gate compiles them out.

### The array arm: rotate ladder, then `pcmpestrm`

The arm shipped first as a rotate ladder structurally identical to NEON's, with `_mm_cmpestrm` declined because its entire case is throughput and throughput was the one thing unmeasurable at the time. **The reasoning was sound and the conclusion was wrong** -- given no hardware, taking the kernel whose correctness argument transfers from NEON was right, and it needed revisiting the moment hardware appeared. Nothing in the code said so; the header presented the ladder as settled.

`intersect_crossover`, `array/generic`, one binary shape apart ( x86 dispatch forced off ):

```text
      m     scalar     ladder   pcmpestrm    est/lad   est/scalar
     32     63.4ns     55.1ns      45.4ns      1.21x        1.39x
    128    389.2ns    166.2ns     148.4ns      1.12x        2.62x
    512   3021.4ns    663.8ns     513.8ns      1.29x        5.88x
   1024   6473.2ns   1380.2ns    1010.6ns      1.37x        6.41x
   4096  33952.0ns   3989.5ns     -- --        1.29x        8.51x
```

`pcmpestrm` is also the *simpler* kernel, which the throughput framing hid: it returns the lane mask directly and all four arms want that mask -- popcount, zero test, accumulator, `SHUFFLE` index. NEON builds the mask from eight compares and narrows it with `lane_bits`; there is nothing to narrow here, so the compaction arm loses a step outright.

Two hazards are written into the code rather than trusted to memory. **Argument order is load-bearing**: the mask indexes the *second* operand, so `contains_all`, the asymmetric arm, takes them in the opposite order from every other arm, and swapping them silently answers a different question ( sabotage-verified red ). And it must be **`pcmpestrm`, not `pcmpistrm`**: the implicit-length form reads a zero element as end-of-string, and `0` is an ordinary chunk value, so it would truncate any block containing one -- faster and wrong.

`SHUFFLE` is **hoisted to the parent module rather than duplicated**. AArch64's `vqtbl1q_u8` yields zero for an out-of-range index and x86's `pshufb` yields zero for a high-bit-set index, and the table's `0xFF` filler satisfies both, so one 4 KiB static drives both compactions.

### Porting the other three arms needed real substitutions

`ops::bitmap` ( AVX2 ), `ops::mixed` ( SSE4.1 ) and `ops::run` ( SSE4.1 ) closed the arch gap at 910 tests on both, with x86 clippy clean under `-D warnings` -- which `scripts/gate.sh` would never check, because it lints the host arch only.

- **No unsigned 16-bit compare.** `min_epu16( lo, hi ) == lo` replaces `cmpgt_epi16`. Sabotaging it to the signed form fails **4** tests, which also proves the corpus reaches above `0x8000`.
- **No lane de-interleave.** `uzp1` / `uzp2` become `packus_epi32` over masked and shifted lanes.
- **`madd_epi16` is the wrong widening step** and no test would have caught it: it reads inputs signed while B6 permits lanes to 65535.
- **ANDNOT reverses its operands** across the arches. Sabotage-verified: 3 tests red.

### A technique's value belongs to the instruction set

Three standard alternatives to the shipped NEON bitmap ladder were measured on the same operands, all four agreeing on 16 535 bits, 300 000 iterations, four repetitions, spread under 1%:

```text
  shipped ladder             98.1 ns      --
  deferred widening         118.6 ns    0.83x
  paired 32-byte loads      144.1 ns    0.68x
  Harley-Seal CSA           283.5 ns    0.35x
```

**Harley-Seal is the textbook answer and loses hardest.** It collapses sixteen vectors to five with carry-save adders so the count runs on an eighth of the data -- worth doing only if counting is the bottleneck. On NEON it is not: `vcntq_u8` is one cheap instruction per 16 bytes, so CSA pays a long XOR/AND/OR dependency chain to save work that was already free. This is exactly inverted on x86, which has no vector popcount at all and must build one from a nibble table and `pshufb` -- which is why the AVX2 arm wins 2.70x over its scalar and why reducing count work pays there.

**Deferred widening** removes three of every four `vpadalq_u8` by summing in `u8` and widening every seventh block, and loses 1.2x -- so the widening accumulate was not binding either. **Paired loads** ( `vld1q_u8_x2` ) halve the load *instruction* count and lose 1.5x, which says the limit is load-port throughput rather than issue width.

The literature corroborates both halves. Muła, Kurz and Lemire, *Faster Population Counts Using AVX2 Instructions* ( The Computer Journal 61( 1 ), 2018; arXiv:1611.07612 ) is the canonical AVX2 result -- Harley-Seal at ~2x hardware `popcnt` and **2.4x** for similarity functions over two bitsets, which is the `and_cardinality` shape, and adopted into LLVM. Muła's own `sse-popcount` repository publishes the aarch64 counter-result, plain NEON CNT beating NEON Harley-Seal at every size ( 0.07334 against 0.11087 at 4 KiB ), with the gap widening. `libpopcnt` corroborates it structurally: its Harley-Seal is copied from that repository **for AVX2**, while ARM gets NEON CNT and SVE is a third algorithm again.

### Where the NEON bitmap kernel sits, and what is left

98.1 ns for 16 KiB is **167 GB/s, about 43 bytes per cycle** at 3.9 GHz -- roughly 2.7 sustained loads per cycle, about 89% of a 3-load/cycle ceiling and 67% of a 4-load one. Remaining headroom is **at most 1.1-1.5x** and none of it is reachable by reducing count work, since all three attempts went backwards. The scalar side is not a straw man either: `w.iter().map( |x| x.count_ones() ).sum()` is auto-vectorized by LLVM on aarch64, so the arm's 1.62x is hand-written NEON beating compiler-generated NEON, and that is what makes the `unsafe` worth carrying.

**What is not ruled out is algorithmic rather than kernel.** These operands are sparse -- 16 535 bits set across 131 072 -- and every arm reads both payloads in full because a dense AND must. A representation letting the intersection skip known-zero regions would beat any amount of kernel tuning, which is a container-format question.

### Packed-view terminals expose a different SIMD problem

A 2026-09-21 disposable probe tested the word loops outside `ops`, on the
native Cortex-X925 performance core with rustc 1.97.1. Each comparison used
seven alternating repetitions, black-boxed operands, and independently checked
answers. The instrument lives under `.agents-workspace/tmp` rather than in
production.

Plain matrix XOR and OR are already done: LLVM emits paired 128-bit loads,
`eor` or `orr`, and paired stores for the ordinary iterator loop. Hand-written
NEON measured 0.99-1.03x from 64 through 16,384 words. This is not room worth an
unsafe arm.

Fused AND-plus-popcount differs. The shipped bitmap accumulation ladder is
1.56-1.83x faster than LLVM's ladder from 256 through 4,096 resident words, but
loses below eight words. In a real `BitMatrix::counted_mul` shape with a fixed
64x64 output, the cloned NEON path was 0.43x current code at a 64-bit inner
dimension, 0.88x at 512, 1.05x at 1,024, **1.38x at 4,096**, and **1.44x at
16,384**. A matrix arm therefore has a case only for wide rows, with a measured
crossover near 1,024 bits; applying it unconditionally would be a large
regression on the common narrow shapes.

The larger result is the interleaved packed view. The fixture had 262,144
logical ordinals, four constituents at 55% density, and 1,048,576 physical bits
held in 16 bitmap containers. The current public operations were compared with
a byte-LUT bitmap prototype and a NEON `tbl` / pairwise-pack prototype:

```text
terminal                 current       byte LUT        NEON       current/NEON
cardinalities           1.361 ms       59.46 us       5.361 us       254x
fold Any                2.499 ms      787.06 us      763.58 us       3.27x
fold All                1.90  ms      115-134 us       85-111 us      17-22x
fold Parity             2.233 ms      475.21 us      449.37 us       4.97x
raw fold reduction          --         28.00 us       5.285 us       5.30x LUT/NEON
```

The fold numbers include rebuilding an `OrdSet`, so output construction hides
most of the 5.3x kernel win for Any and Parity. Cardinalities have no output set
and expose the full gain: NEON is 11.1x faster than the LUT and about 254x faster
than the current set-bit iterator. The large crate-level number is not "SIMD is
254x". Most of it comes from changing the unit of work from one iteration per
set bit plus a residue calculation to fixed-width work over bitmap bytes. SIMD
is the further 11.1x over that representation-aware LUT.

This is a candidate, not a shippable arm yet. The prototype assumes an
interleaved arity of four, a contiguous byte image, and bitmap containers.
Production must dispatch per container, preserve array and run fallbacks,
handle shared unaligned buffers, join output chunks without materializing a
whole physical image, and keep the generic walk as the oracle. Arity 2 and 8
have the same byte-aligned structure; other arities do not. An x86 shuffle arm
needs measurement on real x86 hardware rather than an inferred port. Blocked
views already use range counts and must not be routed through this arm.

### Blocked-view SIMD: cross once per container and pair filters

The first blocked-view SIMD attempt put a feature-gated call around each row.
Its isolated AND-popcount was faster and the endpoint was slower: **6.478 us
against 5.605 us scalar, a 15.6% regression**. The missing cost was the call
shape. A blocked 512-row container crossed that boundary 512 times per filter,
while LLVM had already auto-vectorized the ordinary Rust reducer.

A disposable follow-up under
`.agents-workspace/tmp/simd-jit-revisit-20260921` moved the architecture
boundary outside the entire container and evaluated two filters together so a
data vector was loaded once. The construction was 512 rows, 64 words per row,
two 4,096-bit query masks, nine alternating repetitions pinned to Cortex-X925
CPU 5. Query support of 32 and 4,096 produced the same timings, as expected for
a dense word scan:

```text
shape                         support 32     support 4096
auto-vectorized, per row        10.04 us          10.07 us
ordinary Rust, fused two         9.51 us           9.55 us
NEON, per row                    7.42 us           7.46 us
NEON, whole container            6.24 us           6.25 us
```

That is the dispatch rule now used by the blocked bitmap counter: adjacent
batch filters are paired, the NEON or AVX2 arm is entered once per bitmap
container, and each data load feeds both AND-popcounts. An odd final filter
keeps the scalar row loop. The production Criterion endpoint, 512 rows of
4,096 bits and two sparse filters, measured **7.130 us for the paired batch
against 11.297 us for two one-filter calls, 1.58x**. The untouched one-filter
endpoint was 5.762 us immediately before the change and 5.649 us after it; the
new arm therefore does not reintroduce the original regression.

The AVX2 path was cross-compiled and its direct SIMD-versus-scalar property ran
under `qemu-x86_64`; that establishes correctness only. No x86 throughput claim
is made until it is measured on real hardware. Both architecture arms carry
bound B8 and the property varies row width across vector boundaries and tails,
row count, input words, and nonzero initial output accumulators.

### JIT: vector IR still loses to the existing AOT loop

The same disposable crate compiled `(A & B) | (C & !D)` followed by popcount
through Cranelift 0.135.2. Four equal-length bitmap pointers and a word count
were the complete runtime interface. Each result was checked against optimized
Rust before nine alternating pinned repetitions.

Cranelift did not auto-vectorize the scalar loop. Explicit `i64x2` popcount was
also unsupported by the AArch64 backend, so the vector form used `i8x16`
popcount, unsigned widening through 16-, 32-, and 64-bit lanes, two vector
accumulators, and one final horizontal sum. It improved Cranelift's scalar loop
but did not reach LLVM AOT:

```text
words       LLVM AOT    scalar JIT    vector JIT    AOT / vector
64            15.3 ns       35.1 ns       28.0 ns          0.55x
1,024        217.3 ns      564.6 ns      439.3 ns          0.49x
16,384     4,018.8 ns    9,100.7 ns    7,055.6 ns          0.57x
```

Cold compilation of the first function was 1.050 ms; the second, after process
setup, was 85.8 us. There is **no break-even execution count** because generated
code remains slower at every measured size. Do not add Cranelift to the runtime
for the current view terminals. Re-open JIT only for a demonstrated hot Boolean
DAG where runtime fusion removes enough full bitmap passes to exceed both its
steady-state deficit and compilation cost; compare first with an ahead-of-time
specialized fused loop, since that retains LLVM's better vectorization without
an executable-code cache or new runtime dependency.

### JIT follow-up: fusion wins, but code generation is not yet the main win

The comparison above answered whether Cranelift could beat an already fused LLVM
loop. It did not answer the production question: whether a generated single pass
can beat the nested dynamic `Expr` evaluator. A second experiment on 2026-09-21
used the same `( A AND B ) OR ( C AND NOT D )` cardinality DAG with four 50%-dense,
co-prefix resident bitmap sets. It compared an already planned expression, a
plan-and-run call, eager `OrdSet` composition, a reusable raw multipass loop, an
AOT fused loop over real container payloads, and a JIT fused loop invoked once
per 1,024-word container. Every arm's full answer was checked before timing.

The first explicit vector JIT repeated the widening reduction above and took
439.0 ns per container. Four-way unrolling alone moved it only to 428.1 ns. The
useful change was to accumulate `i32x4` lanes and widen once after the loop; that
reached 324.0 ns. This is safe for the measured cardinality terminal because the
largest possible count over 16,384 words is only 1,048,576, well below `u32::MAX`.
A general implementation still has to derive that bound from the fixed container
size rather than assume it for an arbitrary-length entry point.

Nine alternating release repetitions pinned to Cortex-X925 CPU 5 produced the
following final medians. Allocations are for one execution after all inputs,
plans, and scratch buffers were prepared; the counter was enabled only for the
allocation probe, not during wall-time measurement:

```text
path                    1 bitmap chunk   allocations    16 chunks   allocations
prepared Expr                  661.0 ns            12      8.978 us            42
plan plus run                1,785.4 ns            47     16.029 us            78
eager OrdSet                  439.6 ns             9      9.077 us            66
reusable raw multipass        336.6 ns             0      6.620 us             0
AOT fused, per chunk           224.4 ns             0      4.547 us             0
JIT fused, per chunk           338.8 ns             0      5.770 us             0
AOT fused, flat                213.6 ns             0      4.056 us             0
```

The revised result is narrow but real: JIT fusion is 1.95x faster than the
prepared dynamic evaluator for one chunk and 1.56x for sixteen chunks. A warmed
Cranelift compile took 160.5 us, giving break-even after about 498 one-chunk or
50 sixteen-chunk executions. The first compilation in the process took 1.053 ms,
so charging engine startup instead raises those bounds to about 3,268 and 328
executions. A shape cache is therefore mandatory, and cold or one-shot queries
are not candidates.

The attribution matters more than the headline. The reusable non-JIT multipass
loop is 0.7% faster than JIT for one chunk and 1.96x faster than the prepared
expression without executable code. Across sixteen chunks JIT has a 1.15x
advantage over that multipass loop, while LLVM's ordinary fused Rust remains
1.51x faster than JIT for one chunk and 1.27x across sixteen. The large gap is
removal of boxed stream setup, container allocations, representation selection,
and repeated passes. Runtime
machine-code generation supplies only the residual one-pass advantage, and its
backend emitted worse code than LLVM for that non-canonical IR shape.

This fixture is deliberately favorable to JIT: every operand has every prefix,
every container is a borrowable resident bitmap, all lengths agree, and the
terminal is cardinality. Missing prefixes, arrays, runs, unaligned store-backed
bitmaps, range/complement nodes, and payload decode all require the existing
path or a mixed executor. Current packed-view fold and map terminals already
have representation-aware production kernels; this experiment does not justify
routing them through a generic Boolean JIT.

The implementation order, if a production workload demonstrates repeated hot
DAGs, is therefore:

1. Build a prepared, allocation-free bitmap DAG executor with bounded reusable
   scratch and retain the current generic evaluator as oracle and fallback.
   Measure it first; the raw multipass result says it may capture most of the win.
2. Canonicalize a plan into a stable shape key and record bitmap-eligible chunks,
   fallbacks, bytes visited, executions, and time. Do not compile before measured
   reuse can amortize the warmed compile threshold.
3. Add AOT templates for common normalized shapes before adding a runtime
   compiler. They retain LLVM code quality and need neither writable executable
   memory nor a code cache.
4. Only if the prepared AOT executor still leaves a material residual, prototype
   a cardinality-only Cranelift backend. Cache by canonical DAG, terminal, target
   architecture, and CPU features; bound entry count and executable bytes; use a
   W^X transition; and evict without invalidating in-flight function pointers.
5. Dispatch per aligned prefix. Enter generated code only when all participating
   payloads are borrowable bitmaps; route absent chunks and every other
   representation through the audited kernels. Differential expression tests,
   mixed-representation properties, allocation budgets, and architecture-specific
   direct-call tests remain required.

The decision at this measurement stage was "fusion has a measured break-even,
while JIT itself has not yet beaten the cheaper prepared-AOT options." The
machine-code audit below supersedes the code-quality half of that conclusion.


### Machine-code audit: the large gap was the generated IR shape

The conclusion immediately above was re-opened by disassembling both functions.
LLVM's hot AArch64 loop used exactly the operations expected for a bitmap
popcount reduction:

```text
ldp q?, q?                 paired 32-byte loads
bic / and / or             Boolean DAG
cnt                        byte popcount
uaddlp                     bytes to halfwords
uaddlp                     halfwords to words
uadalp                     words into two independent 64-bit accumulators
```

The first explicit Cranelift generator expressed each widening step as
`iadd( uwiden_low( x ), uwiden_high( x ) )`. That is numerically valid only
because the final operation is a total sum, but it is not Cranelift's canonical
pairwise-reduction shape. The backend therefore emitted `uxtl`, `uxtl2`, and
`add` separately at each level: six instructions after each `cnt`, against
LLVM's two `uaddlp` instructions. Four-vector unrolling multiplied the mistake.
The original generated function was 352 bytes and took about 439 ns per
1,024-word bitmap.

Cranelift 0.135.2 does expose the required operation. Its AArch64 lowering has
an explicit rule for:

```text
iadd_pairwise( uwiden_low( x ), uwiden_high( x ) ) -> uaddlp
```

Using `iadd_pairwise` at both levels, hoisting the four base addresses out of the
unrolled body, and expressing the final horizontal reduction with pairwise IR
changed the result completely. A two-vector loop matching LLVM's four-word
iteration measured in the same pinned run as follows:

```text
1,024-word cardinality                    median
LLVM AOT over arbitrary slices          215.4 ns
LLVM AOT over four fixed-size arrays     213.6 ns
Cranelift JIT, canonical vector IR       217.5 ns
```

The corrected JIT is within 2% of both LLVM controls. Its code is 152 bytes
against 140 bytes for fixed-size LLVM AOT. Over real `OrdSet` chunks the same run
measured 232.4 ns JIT versus 223.4 ns AOT, a 4% difference. At 16,384 flat words
the figures were 4.118 us and 4.042 us. These differences are small enough that
code placement and run-to-run drift matter; there is no longer evidence of a
material steady-state Cranelift penalty for this kernel.

The remaining static differences are understood. Cranelift emits individual
`ldr q` operations where LLVM pairs adjacent loads as `ldp q, q`; LLVM also uses
`uadalp` into two independent `i64x2` accumulators, which Cranelift's public IR
does not currently expose as an accumulating pairwise-long operation. Cranelift
uses an `i32x4` accumulator and an ordinary add. Those differences explain its
12-byte larger fixed loop and the small residual, not the former 1.5-2x gap.

The crucial compiler distinction is therefore **recognition level**, not JIT
versus AOT:

- LLVM starts from a scalar Rust iterator, auto-vectorizes it, recognizes the
  reduction, chooses pairwise-long operations, and schedules independent
  accumulators.
- Cranelift does not auto-vectorize this loop. The generator must provide vector
  IR and must spell target-recognized canonical idioms. Algebraically equivalent
  lane-wise widening is not rewritten into the pairwise form automatically.

This changes the JIT gate. Steady-state code quality is no longer the blocker;
compilation amortization, canonical plan caching, executable-memory lifecycle,
and mixed-container fallback are. The corrected warmed compile took 185.1 us.
Against the prepared expression path it amortized after about 456 one-chunk or
44 sixteen-chunk executions in the final run. AOT templates remain preferable
for common known shapes because they avoid that cost entirely, but an arbitrary

hot bitmap DAG can no longer be rejected on code-quality grounds. Any prototype
must keep architecture-specific IR-shape tests that inspect or otherwise prove
pairwise lowering; a semantically correct fallback to ordinary widening restores
the old regression without failing an answer oracle.

### Parity follow-up: carry independent accumulators

The last steady-state difference was tested rather than assigned to noise. LLVM
carries two independent `i64x2` accumulators and sends one vector result to each
with `uadalp`. The corrected Cranelift loop still combined both vector results
before updating one `i32x4` accumulator, leaving one loop-carried dependency.
The JIT can preserve the same instruction-level parallelism without `uadalp`:
carry two `i32x4` block parameters, add one pairwise-reduced vector to each, and
combine them only in the epilogue.

With two-vector unrolling and those independent accumulators, nine alternating
pinned repetitions produced:

```text
scope                         LLVM AOT    Cranelift JIT    JIT / AOT
1,024 flat words               228.0 ns         215.6 ns        0.95x
16,384 flat words            4.059 us          4.085 us         1.01x
one real bitmap chunk          234.1 ns         230.0 ns        0.98x
sixteen real bitmap chunks   4.690 us          4.806 us         1.02x
```

The direct arms alternated order and reported spreads below 0.6% except for the
unrelated scalar arm. The generated function is 160 bytes versus 140 bytes for
fixed-size LLVM AOT. The extra instructions are still individual loads instead
of `ldp` and ordinary accumulator adds instead of `uadalp`, but they do not
produce a material throughput deficit on this Cortex-X925. Depending on code
placement and run, either arm can lead at one container; over the longer loop the
remaining difference is 0.6%.

This establishes **steady-state performance parity on AArch64 for the measured
bitmap cardinality DAG**. It does not establish x86 parity, parity for materialized
outputs, or end-to-end query parity across mixed containers. The warmed compile
was 191.4 us. Relative to the prepared expression path it amortized after about
455 one-chunk or 44 sixteen-chunk executions, so the production decision remains
a cache and workload decision rather than a kernel-throughput decision.

A production generator should carry at least two independent accumulators for a
popcount terminal, retain canonical `iadd_pairwise` forms, specialize to the
fixed 1,024-word bitmap contract, and test emitted instruction shape per
architecture. A semantic oracle cannot distinguish the slow ordinary-widening
form or the single dependency chain from this parity form.
### ARM SVE: check two preconditions before writing code

`libpopcnt` reports its SVE path beating its NEON one, **"especially on CPUs whose SVE vector width is larger than NEON's 128 bits"**. That qualifier is the whole result. On a Cortex-X925 with SVE and SVE2 both detected, `/proc/sys/abi/sve_default_vector_length` is **16 bytes = 128 bits**, exactly NEON's width -- and since the kernel is load-bound, identical width means identical loads for identical bytes, so there is no mechanism by which SVE moves the binding constraint. Predication would remove the scalar tail, a handful of words out of 1024.

It also cannot ship regardless of width: SVE intrinsics are `stdarch_aarch64_sve`, nightly-only in rustc 1.97.1, against `yesno-core`'s stable MSRV **1.95**. Re-open only on a host where `sve_default_vector_length` exceeds 16 **and** once the intrinsics are stable -- both checkable in one command before any code is written.

### Crate-level numbers are much harder to earn than kernel numbers

Three attempts at a crate-level measurement failed before one worked.

**Two independent `cargo bench` runs differenced by hand** gave `run_x_run` a 1.27x "change" from disabling *bitmap's* arm -- a benchmark that never touches a bitmap container. Noise above 25%.

**A full criterion `--baseline` comparison looked better and was worse**: 339 of 367 benchmarks changed, split 189 slower / 200 faster. Disabling four SIMD arms cannot make 200 benchmarks faster. Criterion's confidence intervals do not help here -- they measure dispersion *within* a run, not drift *between* two runs twenty minutes apart.

**What worked was two binaries, arms on and arms off, run alternately**, which cancels drift into both arms equally. Spreads fell to 3.4-5.6%.

Then an audit invalidated four of the resulting figures, and the fault was the harness in every case:

1. **Wrong path.** `and_cardinality( array, run )` is answered by `ops::card`'s scalar two-pointer; `simd::filter_by_run` is called only from `merge_array_run`, which serves *apply*. Two of the four probe cases never touched a kernel, so a "1.00x, the win vanishes into the container path" was **the arm not being called**. `array x bitmap` likewise reaches no kernel directly.
2. **A hoistable loop.** The timing loop black-boxed the *result* and not the operands, so a pure loop-invariant call could be hoisted straight out, and whether LLVM hoisted it moved with inlining decisions in unrelated modules. That is the real name of the "code layout sensitivity" first recorded -- disabling `ops::array` appeared to make `bitmap x bitmap` 3x faster. The tell was on screen and read past: **96.6 ns for an AND-cardinality over 1024 words is implausible**, and an implausible number is a finding.
3. **Too few iterations.** At 20 000 iterations `bitmap x bitmap` is bimodal from the *same binary* -- eight consecutive runs giving 101.7 ns seven times and 290.1 ns once. The audit's own "baseline 290.2, arm off 169.1" was a corrupted **baseline** beside an honest arm-off number, which **inverted the conclusion**: it said a correct, faster arm was a pessimization, and had it stood the fix would have been to delete working code.

The figures that survive, at 200 000 iterations, six repetitions per side, spread under 0.5%:

```text
  and_cardinality( bitmap, bitmap )   103.5 ns on   167.5 ns off   1.62x
  and_cardinality( array,  run    )  1621.3 ns on  2057.3 ns off   1.27x
  and_cardinality( run,    run    )   131.7 ns on   239.2 ns off   1.82x  ( ops::run )
  array x bitmap                       1637 ns on     1908 ns off   1.17x  ( ops::bitmap )
```

The bitmap figure matching the isolated kernel ( 1.62x popcount, 1.73x `and_cardinality` ) rather than sitting below it is the expected shape for an operation that is almost entirely kernel. `ops::array`'s 8.51x stands: it came from criterion and scaled monotonically with `m`, which a hoisted loop does not do.

### Production bitmap-DAG JIT: measured admission, not kernel-only parity

The production generator originally lived in `yesno-jit`; it moved into
`yesno-core` behind the opt-in `jit` feature on 2026-09-21. Cranelift 0.135
requires Rust 1.95, which core now promises. The default core graph still has
five direct runtime dependencies. Flight's separate `jit` feature enables the JIT;
default server and client-only PostgreSQL transport do not link it. The
temporary `yesno-jit` compatibility re-export was removed on 2026-09-22;
the whole-path benchmark lives in `yesno-core`. The postfix cache key identifies the
Boolean operator tree and each leaf position, and the function takes a pointer
table. The one-prefix cursor holds one `Container` per resident or lazy source
leaf; absent prefixes lend a shared zero bitmap. The JIT counts all-bitmap
prefixes in one vector loop. Other prefixes call core's container kernels;
`Range`, `Not`, oversize DAGs, unsupported architectures and compiler errors
fall back to the ordinary expression. Source errors propagate as errors.

**Superseded on 2026-09-21.** The seed stride below made every pair of the
first eight leaves disjoint: `a & b` was empty and `c \ d` was just `c`.
The old comparison also omitted planning on the core side while JIT still
planned per call. Keep the figures as the failed measurement, not as evidence
for JIT admission or kernel parity.

Whole-expression benchmark: `cargo bench -p yesno-jit --bench dag` on this
AArch64 host. For each of 1, 16, 64 and 256 aligned chunks, construct four
`OrdSet`s with 6,000 generated ordinals per chunk using
`( i * 0x9e3779b9 + seed * 0x85ebca6b ) & 0xffff` with seeds 1-4. Compare
`( a & b ) | ( c \ d )` after planning, using 9 repetitions of
`max( 64, 8192/chunks )` iterations per arm; compile once before timing,
black-box the prepared expression on every iteration, and report per-arm
medians. Two successive runs of the same anti-hoisting binary:

```text
chunks     prepared core / cached JIT (ns)         core / JIT
    1                528.9 /     908.6              0.58x
   16               6994.1 /    6564.5              1.07x
   64              29164.0 /   27901.9              1.05x
  256             154733.7 /  148511.9              1.04x
    1                526.2 /     911.7              0.58x
   16               6971.1 /    6606.9              1.06x
   64              28891.0 /   26533.1              1.09x
  256             133505.8 /  119021.1              1.12x
```

This is a modest *whole-query* benefit, not the parity claim from the raw
kernel. The 256-chunk saving fluctuated from 6.2 to 14.5 us; amortizing the
prototype's roughly 190-us compile takes about 13-31 executions, and neither
the prototype compile cost nor the warmed timing is an SLA. Auto-admission
therefore requires a leaf reporting at least 256 chunks; small view elements
use core's faster evaluator. Explicit `DagJit` can still compile smaller
repeated workloads. A thread-local 64-shape cap bounds executable mappings;
x86 defaults to fallback until measured. This leaves materialized-result
terminals, x86 vector lowering, and an adaptive reuse policy open.

An integration test revealed an independent correctness bug: the planner
returned `None` for `bounds( Source )` when a source did not report a prefix
span, while its identity rewrites read `None` as *proven empty* and erased it.
The source now receives a conservative whole-universe bound; an independent
eager-set oracle tests all four Boolean operators. The JIT's property tests
also check its compiled ABI, sparse-prefix alignment and lazy read errors.

### Corrected nested-DAG benchmark ( 2026-09-21 )

The benchmark now lives at `yesno-core/benches/dag.rs` and runs with
`cargo bench -p yesno-core --features jit --bench dag`. On this AArch64 host
( Rust 1.97.1, Cranelift 0.135.2 ), each of 16 resident leaves has 6,000
draws per chunk. Draw `i` in prefix `p` of seed `s` uses the low 16 bits of
`mix64( s * 0x9e3779b97f4a7c15 + p * 0xd1b54a32d192ed03 + i )`,
with wrapping arithmetic and SplitMix64 finalization. Every resulting
container is asserted Bitmap, every adjacent seed pair overlaps, and each
result is checked against the ordinary collected expression before timing.
The six shapes are:

- `mixed4`: `( a & b ) | ( c and_not d )`.
- `balanced8`: `(( a & b ) | ( c & d )) xor (( e | f ) & ( g and_not h ))`.
- `deep8`: left-associate `xor b`, `and_not c`, `or d`, `and e`, `xor f`,
  `or g`, `and_not h`, starting at `a`.
- `or8`: left-associated union of eight leaves.
- `xor8`: nested exclusive-or of eight leaves.
- `mixed16`: `balanced8( a..h ) and_not balanced8( i..p )`.

Both timed arms receive the **same unplanned expression**. Core calls
`Expr::cardinality()` and JIT calls a warmed `DagJit::try_cardinality()`;
both therefore plan once per invocation. Nine batches per arm alternate
order, each batch running `max( 64, 8192 / chunks )` iterations with
black-boxed input and result. Each cell below is the min-max speedup across
three complete benchmark runs; the final column gives run 3's 256-chunk
medians in microseconds:

| shape | 1 chunk | 16 chunks | 64 chunks | 256 chunks | 256 core / JIT ( us ) |
|---|---:|---:|---:|---:|---:|
| mixed4 | 3.05-3.10x | 5.04-5.09x | 6.58-6.65x | 7.86-8.81x | 1094 / 127 |
| balanced8 | 4.14-4.24x | 11.53-11.68x | 16.99-18.28x | 11.63-15.39x | 4295 / 282 |
| deep8 | 2.32-2.36x | 4.08-4.11x | 6.19-6.66x | 5.21-6.66x | 1815 / 277 |
| or8 | 1.56-1.61x | 1.66-1.72x | 1.59-1.61x | 1.60-1.62x | 430 / 265 |
| xor8 | 1.47-1.50x | 1.89x | 1.86-1.92x | 1.72-1.81x | 518 / 289 |
| mixed16 | 4.05-4.13x | 21.58-21.99x | 18.92-22.79x | 12.60-15.07x | 12016 / 805 |

Mixed trees benefit most because core re-enters dynamic stream operators
while JIT fuses their bitmap words; the existing n-ary union path keeps
`or8` relatively close. The 256-chunk ratios move noticeably between
complete runs ( especially `balanced8` and `mixed16` ), so one run's
point estimate must not be used as an admission threshold. First-call
observations include compilation **and execution**, ranging from about
0.11 to 1.36 ms here; they are not isolated compile timings. These are
whole-query, same-input measurements of the shipped API paths, **not** a
raw generated-loop versus LLVM-kernel comparison. The old 256-chunk
auto-admission rationale needs remeasurement against reuse and shape; this
benchmark alone does not justify changing the production threshold.

### Nested-DAG kernel-only AOT comparison ( 2026-09-22 )

The corrected whole-query benchmark cannot isolate code generation from
planning and stream execution. A separate research binary at
`.agents-workspace/tmp/simd-jit-revisit-20260921/src/bin/nested_dag.rs` compares
fused kernels for the same six shapes on AArch64 ( Rust 1.97.1 ). It copies the
production Cranelift postfix lowering, including its two-way vector unroll and
fixed 1,024-word loop, into a standalone instrument; it does **not** call a
private production kernel or add a measurement API to core. The LLVM AOT arm
uses one statically compiled fused loop per shape. Both arms have the same
`extern "C" fn( *const *const u64 ) -> u64` ABI, read the same pointer tables,
and are invoked by the same outer per-chunk loop. Compilation is outside timing.

The bitmap words are drawn with exactly the seed, prefix and SplitMix64 formula
of the shipped benchmark, with 6,000 draws per leaf and chunk. Each leaf is
checked to be bitmap-shaped, adjacent leaves overlap, and every AOT and JIT
answer equals an independent scalar postfix walk. All 24 whole-query result
counts also matched the shipped benchmark exactly. For each arm, nine batches
alternate order with `max( 64, 8192 / chunks )` whole-corpus invocations;
the input pointer table and result are black-boxed. Three complete runs give
these min-max *AOT time / JIT time* ratios ( below 1 means AOT is faster ):

| shape | 1 chunk | 16 chunks | 64 chunks | 256 chunks |
|---|---:|---:|---:|---:|
| mixed4 | 0.810-0.811 | 0.953-0.958 | 0.963-0.972 | 0.998-1.006 |
| balanced8 | 0.920-0.971 | 1.010-1.018 | 0.983-1.022 | 1.010-1.026 |
| deep8 | 1.005-1.017 | 1.004-1.023 | 0.994-1.013 | 0.977-1.008 |
| or8 | 0.916-0.955 | 1.004-1.017 | 0.996-1.000 | 1.001-1.013 |
| xor8 | 0.960-0.999 | 1.008-1.029 | 0.996-1.006 | 0.965-1.037 |
| mixed16 | 0.904-0.906 | 0.867-0.874 | 0.930-0.984 | 0.987-1.026 |

At 256 chunks the raw kernels are within about 4% for every shape. This is
the parity claim the earlier four-leaf prototype could not support across
nested DAGs. Small workloads retain real AOT advantages: `mixed4` uses about
19% less AOT time at one chunk and `mixed16` about 13% less at 16 chunks.
Absolute 256-chunk times moved sharply between complete runs ( for example,
`balanced8` AOT 297 -> 456 -> 449 us ), while paired ratios stayed close;
the near-parity conclusion is stronger than any single absolute latency.

An anti-hoisting check temporarily black-boxed every output word inside only
the AOT `mixed4` kernel, without changing its answer. Its one-chunk time rose
from about 217 to 608 ns and its 16-chunk time from about 4.45 to 9.95 us;
the other AOT shapes and the `mixed4` JIT arm stayed near their baselines.
The scratch source was restored before recording the ratios above.

The 1.6-15x whole-query JIT wins above are therefore primarily wins from
fusion over core's dynamic stream evaluator, not evidence that Cranelift emits
faster fused instructions than LLVM. The raw experiment is a generator copy,
not the shipped entry point, and cannot attribute small-query overhead among
planning, cursor setup and kernel invocation. It does not by itself change the
automatic admission threshold or prove compile-cost amortization.

### Whole-query cold-cache admission study ( 2026-09-22 )

The scratch harness at .agents-workspace/tmp/jit-admission-20260922/
reused the six overlapping-bitmap DAGs above ( SplitMix64, 6,000 draws
per leaf and chunk ). It passed the same unplanned Expr to core
cardinality and DagJit on AArch64 with Rust 1.97.1. No production code
or admission policy was changed. The JIT source SHA-256 was
2a9b8039b8bf32260e2afa019ed6e75916b620402df3eb096a5146a918cbf960
throughout the sweeps.

Each cold sample used a fresh DagJit and timed planning, compilation and
execution together; eleven samples produced a median. Warm JIT and core
used eleven alternating batches of max( 64, 8192 / chunks ) calls each.
All answers agreed. Estimated break-even calls were calculated as
ceil( ( cold - hot ) / ( core - hot ) ), floored at one; this assumes reuse
of one shape cache and is not a separately timed N-call batch. Three
complete sweeps covered 1, 16, 64 and 256 chunks; two more added 8, 32
and 128 chunks. Observed break-even call ranges:

| shape | 1 | 8 | 16 | 32 | 64 | 128 | 256 chunks |
|---|---:|---:|---:|---:|---:|---:|---:|
| mixed4 | 34-85 | 5 | 3 | 2 | 1 | 1 | 1 |
| balanced8 | 16 | 2-3 | 1 | 1 | 1 | 1 | 1 |
| deep8 | 35-37 | 5-6 | 3 | 2 | 1 | 1 | 1 |
| or8 | 88-90 | 27 | 12 | 7 | 4 | 2 | 1 |
| xor8 | 104-114 | 19-20 | 8-9 | 4 | 2-3 | 1-2 | 1 |
| mixed16 | 13-14 | 1 | 1 | 1 | 1 | 1 | 1 |

At 16 chunks, balanced8 took about 106-109 us cold versus 149-151 us
for one core call; mixed16 took 213-220 us cold versus 639-646 us core.
Flat or8 and xor8 instead needed about 12 and 8-9 calls. At 64 chunks,
nested shapes repaid compilation on the first call, but or8 still needed
about four. At 256 chunks, all tested shapes reached estimated first-call
break-even, though or8 had only a small margin.

Decision: retain the 256-chunk automatic gate for now. Lowering it
globally would eagerly compile flat OR/XOR queries with costly first use.
Before shipping a shape-aware gate, time actual fresh-cache N-call batches
near the crossovers and vary bitmap density, key alignment and container
types. The one-chunk mixed4 cold median varied from 69 to 171 us across
full sweeps, showing why one compile timing is insufficient.

### Peer audit: chunk-count admission does not bound traversal ( 2026-09-22 )

The cold-cache table above is a result for six dense, overlapping bitmap
DAGs, not a general endorsement of automatic JIT at 256 chunks. A separate
resident-backed ChunkSource probe found that an AND with one 256-chunk
bitmap leaf and one matching one-chunk bitmap leaf decoded two payloads
through core but 257 through automatic JIT. Across five warm alternating
batches of 100 calls, core took 0.506-1.064 us and automatic JIT
69.681-183.002 us, with equal counts. The JIT's union-of-prefixes walk
does not inherit core's seek-driven intersection. A 256-by-256 sparse-array
AND also took 14.83-14.95 us in core versus 32.52-32.69 us through
automatic JIT; no bitmap JIT kernel ran. Even one aligned bitmap AND took
about 85-86 us in core versus 103 us through JIT. Thus the 256-chunk
gate is not a workload safety bound, and shape, selectivity, encoding
and traversal must be considered before changing automatic admission.
The earlier recommendation to retain the gate is superseded for these
operand classes; production code was not changed by this review.

The same audit exposed two independent correctness/lifetime issues.
Selective ViewIntersectionCounter computes chunk_end in u64 and overflows
at the final legal prefix: for an interleaved one-way view and a filter
containing u64::MAX - 1, FullScan returned [[1]] while Selective
returned [[0]] in release. Preserve the exclusive endpoint with u128 or
carefully justified saturation and add boundary coverage. Cranelift
0.135.2's SystemMemoryProvider leaks executable allocations on Drop
unless JITModule::free_memory is called. Fresh DagJit compile/drop
cycles increased executable virtual mappings by 4 KiB per cycle in the
peer probe. The 64-shape cap does not bound that churn; a fix must cover
successful and failed compilation paths and ensure no function pointer
outlives the mapping. These are open findings, not fixed by the
map/fold scratch prototype.

## Files

- `yesno-core/src/ops/array.rs` -- the NEON and x86 ( `pcmpestrm` ) arms, the shared `SHUFFLE` table in the parent module, and the direct-call test wrappers.
- `yesno-core/src/ops/bitmap.rs` -- the NEON ladder and the AVX2 arm.
- `yesno-core/src/ops/mixed.rs`, `yesno-core/src/ops/run.rs` -- SSE4.1 arms beside their NEON ones.
- `yesno-core/src/ops/card.rs` -- which pairs are answered by a scalar two-pointer rather than by any arm, which is what a crate-level probe must trace before timing.
- `yesno-core/benches/setops.rs` -- `intersect_crossover`, whose `array/generic` column is current behaviour and not a counterfactual.

## Test Coverage

- 910 tests pass on aarch64 and on x86_64 under `qemu-x86_64`. The direct-call test wrappers are compiled for **both** arches, because their purpose is reaching a kernel the dispatcher would skip on a host lacking the feature.
- Every ported substitution is sabotage-verified with the count of reddened tests recorded: signed compare 4, ANDNOT 3, dropped rotation 3, `is_disjoint` forced true 1, `contains_all` forced true 1, swapped `contains_all` operands red.
- Every performance variant asserts the same answer before being timed.

## Pitfalls

- **Never quote an emulated timing.** It can invert the verdict, not merely blur it.
- **A green test suite is not evidence a kernel was reached.** Sabotage each arm in turn. Wrap a sabotage loop in `trap ... EXIT INT TERM`: one hit the 2-minute tool timeout between corrupting a file and restoring it, and a restore that runs only on the success path is not a restore.
- **A sabotage must be semantically wrong and structurally identical.** Replacing an adapter with the sequential default made a field unused, so the build died in clippy ( exit 101 ) before a single assertion ran, and the test stayed unmeasured.
- **Trace which kernel a benchmark actually calls before timing it.** Two of four probe cases in one harness reached no kernel, and a null result from a benchmark that never executed the code under test is indistinguishable from one that did.
- **Black-box the operands, not the result**, or the call hoists out of the loop and the measurement tracks inlining decisions in unrelated modules.
- **Sabotage is necessary and not sufficient for a performance claim.** The arm-off run did move; the number it moved *from* was noise. State the spread, and repeat until the spread supports the claim.
- **Single-module response is the test that separates signal from artifact.** A figure that moves when an unrelated module's arm is disabled is a build artifact, whatever its size.
- A doc claiming a benchmark column measures the *absence* of an arm, while the column measures its presence, survives until someone has a second build to compare against. `intersect_crossover`'s `array/generic` was described as a counterfactual for three weeks after `ops::array::and_cardinality` landed.
- Do not port a win across arches on the strength of the algorithm's name.

## 2026-09-22: Conservative automatic-JIT gate retains the dense win

After the admission change, a one-off release probe under
`.agents-workspace/tmp/jit-admission/` compared three *whole-expression*
paths on this AArch64 host: `Expr::cardinality`, automatic
`jit::cardinality`, and warm explicit `DagJit::try_cardinality`. The expression
was `(A AND B) OR (C ANDNOT D)` with four resident leaves. Each leaf held
exactly 256, 512, or 1,024 contiguous prefixes, with 5,000 distinct scattered
lows per prefix; the probe asserted every container was a bitmap. Values were
formed from an odd multiplicative low-bit permutation plus a leaf seed. All
three paths returned the same count before timing. Each sample ran 200 calls
with the expression operand black-boxed at every call; seven samples were
taken per path, sorted, and the median reported. An initial probe that only
black-boxed the result was discarded. The cache was warmed before the samples.
Three independent process runs followed, with the same path order each time;
these are not randomized paired trials.

| Chunks/leaf | Core median (us) | Auto JIT median (us) | Explicit JIT median (us) | Auto/core |
|-------------|-----------------:|---------------------:|-------------------------:|----------:|
| 256, run 1 | 191.1 | 127.6 | 127.2 | 0.668 |
| 256, run 2 | 199.3 | 127.2 | 126.9 | 0.638 |
| 256, run 3 | 191.8 | 131.0 | 128.9 | 0.683 |
| 512, run 1 | 477.3 | 306.3 | 305.7 | 0.642 |
| 512, run 2 | 476.5 | 307.9 | 303.8 | 0.646 |
| 512, run 3 | 481.4 | 306.2 | 349.9 | 0.636 |
| 1,024, run 1 | 1,359.5 | 925.8 | 890.2 | 0.681 |
| 1,024, run 2 | 1,313.0 | 934.3 | 886.0 | 0.712 |
| 1,024, run 3 | 1,317.7 | 907.3 | 893.8 | 0.689 |

The automatic gate's resident-container scan did not consume the dense win
on this fixture: its ratio to core stayed between 0.636 and 0.712. This does
not price source-backed planning, sparse overlap, x86, or compile break-even.
The separate counted-source regression establishes that the admitted-shape
restriction avoids the previously observed 257-versus-2 payload-read loss;
it is a work counter, not a wall-clock claim.

### Binary admission follow-up ( 2026-09-22 )

The peer independently reran the three earlier reproducers after the fixes:
terminal-prefix Selective and FullScan both returned `[[1]]`, executable
mapping size stayed flat through 40 new `DagJit` compile/drop cycles, and
automatic selective AND read two payloads rather than 257. A truthful
`all_bitmap_chunks()` hint then exposed a different losing shape. Two
aligned 256-chunk bitmap `ChunkSource` leaves under plain binary AND, in five
warm rotated batches of 100 calls, measured 86.053-86.324 us through core,
105.282-106.680 us through automatic JIT, and 104.092-105.137 us through
explicit warmed JIT. The counted sources were resident-backed; all answers
agreed. The custom hint scanned tags per call, unlike `KeySource`'s cached
hint, but explicit JIT's similar timing shows that hint work alone does not
explain the loss. Raw peer data and its source are in the haiiie checkout's
`.agents-workspace/tmp/jit-review-20260922/benchmark-hinted-after.csv` and
`hinted` probe. This is not a source-backed or end-to-end scoring result.

Automatic admission now checks for at least four expression leaves before
container scans or source metadata queries, leaving simple binary cardinality
with core. The four-leaf `( A AND B ) OR ( C ANDNOT D )` fixture remains
eligible, preserving the prior whole-expression win measured above. Explicit
`DagJit` is unchanged and still has union-draining behavior for selective
inputs. Three-leaf shapes have no positive whole-path evidence and also stay
on core automatically.

Post-change, the existing AArch64 release probe at
`.agents-workspace/tmp/jit-admission/` rechecked the same four-leaf resident
set construction ( 5,000 scattered lows per prefix, all bitmap containers ).
Seven samples of 200 calls per path, with the expression black-boxed at each
call and a warmed cache, produced these single-run median whole-expression
times; path order was fixed, so this is a preservation check, not a new
cross-host threshold study:

| Chunks/leaf | Core ( us ) | Automatic JIT ( us ) | Auto/core |
|-------------|------------:|---------------------:|----------:|
| 256 | 240.446 | 169.267 | 0.704 |
| 512 | 680.444 | 468.963 | 0.689 |
| 1,024 | 1,748.706 | 1,131.411 | 0.647 |

### Seek-driven explicit fused-DAG traversal ( 2026-09-22 )

The binary admission fix was independently retimed with truthful
`all_bitmap_chunks()` source hints before this traversal change: five rotated
100-call AArch64 batches gave core 87.346-92.167 us, automatic JIT
87.426-88.676 us, and warmed explicit JIT 106.065-108.159 us for aligned
256-chunk binary bitmap AND. Automatic selective AND still read two payloads;
explicit still read 257. Raw peer results are
`/home/moriyoshi/src/haiiie/.agents-workspace/tmp/jit-review-20260922/benchmark-hinted-binary-gate.csv`.

Explicit `DagJit` now evaluates a lower-bound prefix for the *whole* postfix
DAG, seeks lagging streams, and loads payloads only for leaves active at the
candidate. A source may return a later chunk after a loose `peek_prefix`;
one pending chunk per leaf is buffered, and a seek/consume floor prevents a
loose bound from moving backward. AND takes the later child bound, OR/XOR the
earlier, and ANDNOT follows the left while checking a coincident right.
Counted-source regressions failed on the old executor at 256 wide-side
payload reads and pass at one for both binary AND and an AND branch below OR.
A lower-bound-peek regression failed at count 2 versus oracle 4 when pending
later chunks were deliberately dropped, then passed after restoration. The
128-case randomized Boolean-DAG/core property uses a common compiled prefix
plus independently selected gaps at 1, 128 and 255.

Automatic JIT still uses its previously measured dense union walk; its equal,
contiguous all-bitmap and four-leaf gate makes that walk appropriate, and the
x86 admission measurement was taken on that path. The explicit seek executor
shares the audited bitmap-call helper but does not widen the automatic gate.
The scratch release probe at `.agents-workspace/tmp/jit-selective/` used
resident-backed `ChunkSource` wrappers ( cached chunk count, span and bitmap
hint ) around a 256-prefix wide bitmap and one matching prefix 128, with
5,000 scattered lows per chunk. Each complete process run warmed one
`DagJit`, then took six batches of 500 calls per arm in alternating order;
each call paid normal planning and the expression operand was black-boxed.
Three final-form AArch64 runs yielded core medians 0.731, 0.331 and 0.298 us
versus explicit JIT 1.253, 0.521 and 0.541 us, ratios 1.713, 1.572 and
1.816. The work blow-up is gone, but core still wins this selective shape;
automatic admission must remain closed to it.

The existing dense four-leaf `( A AND B ) OR ( C ANDNOT D )` probe kept its
win after the split. Its resident sets have 5,000 scattered lows per chunk;
seven samples of 200 black-boxed whole-expression calls per arm were taken in
each of three complete AArch64 release runs. Automatic/core median ratios
were 0.675-0.688 at 256 chunks per leaf, 0.646-0.675 at 512, and
0.702-0.721 at 1,024. Explicit seek/core ratios were 0.700-0.703,
0.672-0.707 and 0.708-0.752 respectively. The path order was fixed, not
randomized; compare ratios across runs, not absolute times across run batches.
