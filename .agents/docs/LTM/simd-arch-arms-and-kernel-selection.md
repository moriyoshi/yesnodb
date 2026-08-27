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

### ARM SVE: check two preconditions before writing code

`libpopcnt` reports its SVE path beating its NEON one, **"especially on CPUs whose SVE vector width is larger than NEON's 128 bits"**. That qualifier is the whole result. On a Cortex-X925 with SVE and SVE2 both detected, `/proc/sys/abi/sve_default_vector_length` is **16 bytes = 128 bits**, exactly NEON's width -- and since the kernel is load-bound, identical width means identical loads for identical bytes, so there is no mechanism by which SVE moves the binding constraint. Predication would remove the scalar tail, a handful of words out of 1024.

It also cannot ship regardless of width: SVE intrinsics are `stdarch_aarch64_sve`, nightly-only in rustc 1.97.1, against `yesno-core`'s stable MSRV **1.89**. Re-open only on a host where `sve_default_vector_length` exceeds 16 **and** once the intrinsics are stable -- both checkable in one command before any code is written.

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
