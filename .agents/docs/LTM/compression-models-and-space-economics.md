# Compression Models and Space Economics

## Summary

The storage-format investigation uses information-theoretic bounds and measured chunk-shape histograms to distinguish representational opportunity from implementation priority. Cardinality alone explains the broad pressure on arrays and bitmaps, while run count explains why chunks with the same cardinality can have very different compressibility. The measurements justify further format research, and they have now been used to decline one concrete proposal: a fourth container kind backed by a compressed binary trie was costed on both axes in August 2026 and rejected.

The instrument that produced all of this, `yesno-core/src/stats.rs`, was **removed from the tree on 2026-08-28** once that decision was made. Its 978-line pre-trie implementation is recoverable at commit `7a641fb`; the trie modelling was never committed and survives only in this document and in the 2026-08-28 `JOURNAL.md` entries. Anything that needs to re-ask an encoding question must rebuild it from those two sources.

## Key Facts

- For a universe of `n = 65,536` values and cardinality `m`, the cardinality-only lower bound is `H(m) = log2 C(n, m)` bits.
- The largest sparse-interior gap measured at `m = 4,096` was about `2.97x`; dense-interior bitmap gaps reached about `4.86x` at `m = 63,422` and `5x` at the full-chunk boundary.
- A complemented-array representation would improve dense chunks, but its peak gap is still about `2.51x`; it mainly repairs the exact dense boundary rather than solving the general interior problem.
- Cardinality is not a sufficient shape model. For `r` runs, the number of bitmaps is `N(m, r) = C(m - 1, r - 1) C(n - m + 1, r)`, which makes run count a useful second coordinate.
- Very wide ratios can be economically irrelevant. At `r = 1`, the absolute waste may be only four to six bytes, so investigations should rank byte savings before ratios.
- The first measured histogram covered 430 chunks: about 475.5 KiB stored versus a 280.2 KiB lower bound, leaving about 195.3 KiB of representational waste, or `1.70x` overall.
- In that sample, sparse arrays accounted for about 49.3% of waste, dense bitmaps for about 50.5%, and contiguous chunks for about 0.2%.
- Computing run count for arrays or bitmaps requires walking payloads. It is appropriate for offline measurement, but it is not free metadata for a hot planning path.
- Chunk-index overhead must be priced with payload compression. For cardinalities at or below three, the index can represent essentially all stored bytes, and the current per-entry floor is about ten bytes.
- That index cost buys direct lookup and stable `ChunkRef` metadata. A smaller representation that loses those properties is not automatically an improvement.
- Elias-Fano, RRR-style bitvectors, recursive partitioning, and compressed-domain operations are relevant comparison points, but their navigation and mutation costs must be evaluated against this crate's workload.
- The compressed binary trie of `formal-model.md` §13.6 is the one published structure that beats Roaring on space and time simultaneously. Costed here it saves **6.6%** as a fourth kind and **loses 20.1%** as a wholesale replacement, on the reference corpus. Since the cost model omits the `o(trie(S))` term, 6.6% is an upper bound, and §13.9's own threshold is that about 6% does not justify a format change.
- The two figures differ because a fourth kind is only ever selected where it wins. Price a candidate encoding **per chunk against the encoding already chosen**, not as a replacement for all of them. Taking the minimum of the two totals rather than the sum of the per-chunk minima understates the win.
- Trie space is governed by node count. Plain form costs `2(nodes - m + 1)` bits, where the leaf level is free because every leaf sits at known depth 16. Run-compressed form collapses entirely-present dyadic subtrees and costs `2 * pruned_nodes`, with no leaf discount because pruned leaves sit at varying depth.
- Run compression is therefore **not uniformly cheaper**. Where no dyadic subtree is full it costs `2(m - 1)` bits more than the plain form. Any model of it must take the better of the two per chunk.
- Plain node count has a closed form needing one `O(m)` pass and no descent: `nodes = 1 + D*m - sum over adjacent pairs of lz16(v[i] ^ v[i-1])`, for universe `2^D`.
- Measured trie economics by regime, one scattered chunk each. Arrays cross over near `m = 270`: `-84%` at `m = 4`, `+13.2%` at `m = 600`, `+46.7%` at `m = 4,096`. Bitmaps lose across the whole mid-density band, `-25.2%` at `m = 16,384` and `-93.6%` at `m = 49,152`, recovering only past 90% density. Many-short-runs is its best regime, `+59.4%` at 512 runs of 6 and `+81.2%` at 2,000 runs of 2.
- A bitmap cannot be beaten where a bitmap is already correct. The ratio `trie / bitmap = 2d(log2(1/d) + 1)` depends only on density `d`, **not on chunk width**, so the mid-density losses hold at every chunk size.
- Widening the chunk does not rescue a trie. From `D = 16` to `D = 20` the trie's margin grows from 15% to 43% at `N = 1e7`, while total bytes per ordinal moves 1.71 to 1.70, because the array baseline degraded by the same amount.
- Run containers occur in a narrow, non-monotonic band of corpus shapes. Measured over five shapes: contiguous ingest gives all `r = 1`; bursty data with runs of 4 to 64 gives run counts of 95 to 1790, holding ~100% of Run payload bytes; and uniform-scattered at 1%, 10% and 40%, plus an aged point-edit workload, give **no Run containers at all**.
- The band is non-monotonic in burst length and no analysis predicted it. Runs of **2** adjacent ids produce *no* run containers, because the implied interval count loses to a bitmap and `optimize()` never picks `Run`; runs of 16 land near `r = 1790`, just under `RUN_MAX_INTERVALS`; runs of 64 fall back to `r ≈ 784`. The shape that looks most runny produces no runs.
- Consequently, a run-kernel optimization above ~64 intervals pays on clustered or series-structured corpora and nothing on scattered, aged-by-point-edit, or purely append-shaped ones — where the encoding is already `r = 1` or is not `Run` at all.
- The aged-state scenario cannot answer questions about container shape. It reports slab classes, and the harness exposes container kind and length but no run count. At 400 keys x 400 ordinals over a 100 000 spread it allocates **only the index class**, with the index at 45.9% of bytes at 1% churn and 30.6% at 5%, and space amplification 1.50x and 2.00x.
- Chunk width is not a tuning parameter in any case. Larger chunks win about 15% in the very sparse regime, entirely by amortizing index entries, are about 18% worse near `N = 1e6`, and are flat elsewhere. `per-key-blob-for-cold-keys` attacks the same term at 2.5x using a reserved format bit.

## Details

The useful distinction is between a bound and a design. `H(m)` identifies how much redundancy remains when only cardinality is known; it does not say which format can achieve that bound while preserving fast set operations. Adding run count produces a more discriminating model, especially for clustered data, but also increases the metadata or scanning cost needed to choose a representation.

The measured histogram divides the opportunity almost evenly between sparse and dense chunks. That argues against a single local patch such as complemented arrays being treated as the general answer. It also means payload-only comparisons can mislead: tiny chunks are frequently dominated by index entries, while dense chunks may benefit from representations whose query costs differ substantially from plain bitmaps.

Any proposal for a new container or index encoding should therefore report total bytes, absolute bytes saved, lookup and iteration behavior, conversion costs, and whether union, intersection, difference, and cardinality can operate without materialization. The corpus and workload must be representative before a runtime format decision is made.

## Files

- `yesno-core/src/stats.rs` **no longer exists**. It was removed on 2026-08-28 once the container-kind question it gated had been answered. The **complete source as removed** is preserved verbatim in [Removed source: `stats.rs`](./removed-stats-instrument-source.md), which is the only copy of the trie modelling; the pre-trie implementation is also at `git show 7a641fb:yesno-core/src/stats.rs`.
- Rebuilding it needs three constants that encode decisions rather than taste. The bound is `bound_bits = log2 C(m-1, r-1) + log2 C(n-m+1, r)`, unrealizable when `r = 0`, `r > m`, or `r > n-m+1`. The cardinality bin edges were `[1, 4, 16, 64, 256, 1024, 3584, 4096, 4097, 8192, 16384, 32768, 49152, 61440, 64512, 65535, 65536]`, giving `ARRAY_MAX` and the full chunk their own bins. The run-count edges were `[1, 2, 3, 5, 9, 17, 33, 65, 129, 257, 513, 1025, 2033, 4097, 8193, 16385]`, isolating `r = 1` and placing a boundary just past `RUN_MAX_INTERVALS`.
- `docs/formal-model.md` records the mathematical model and proof obligations. **Corrected 2026-09-07**: this line named `yesno-core/src/formal_model.rs`, a file that has never existed in this repository's history. The model is a *document*, not a module, and stating it as source sent a reader looking for a proof obligation in code that was never going to have one. Note the contrast with the `stats.rs` line above, which names a removed file and **says so** — that one is a correct historical reference. A name asserted in the present tense is a claim, and this one was false.
- The extent, segment, and index modules define the storage overhead that must be included in comparisons.
- Measurement scenarios under `e2e/scenarios/` provide reproducible corpus observations.
- `.agents/docs/JOURNAL.md` contains the original derivations, measurements, and literature notes.

## Test Coverage

- Storage codec and differential tests protect byte compatibility for existing representations.
- Expression and set-operation properties protect semantics if a compressed-domain operation is introduced.
- Allocation tests constrain implementations that appear compact on disk but materialize during queries.
- Corpus fixtures should report both payload and index bytes so a format experiment cannot hide overhead in another layer.

## Pitfalls

- Do not prioritize a large compression ratio without checking the absolute bytes it saves.
- Do not compare measurements produced by different setup paths as though they shared a corpus.
- Do not use an offline `(m, r)` histogram directly as a planner signal without pricing the payload walk.
- Do not add a new container kind before representative corpus measurements show that its total economic benefit exceeds its complexity and dispatch costs. Going from three kinds to four takes the kernel matrix from 9 pairs to 16, which is up to 49 new arms across the four binary operations plus `and_cardinality`, `is_disjoint`, and `contains_all`. A missing arm returns the right answer slowly, so no correctness layer can see it and only a benchmark naming the pair can.
- Do not re-propose the compressed binary trie from the published headline numbers. Those are intersection-only, static, and measured at a `2^25` to `2^26` universe. Re-run the instrument on the corpus in question first.
- Do not treat a growing advantage ratio as a growing win. A trie's margin improves with chunk width only because the array baseline it is measured against degrades at the same rate. Check both terms of a ratio for movement before reporting it.
- Do not bin a chunk that lives inline in its index entry. At or below `INLINE_MAX` a non-bitmap chunk owns no payload extent, so a payload-bytes accessor reports bytes that are not stored anywhere, and binning them at zero distorts every ratio and every waste ordering.
- Do not accumulate a bound after binning. Evaluate it per chunk at the exact `(m, r)` so that totals are exact and bin edges remain a display device.
- Do not order a waste table by ratio. A full chunk has a bound of exactly zero and therefore infinite ratio against six bytes of real waste, and the perfectly alternating chunk reaches a ratio of 4,369 while being measure-negligible in any real corpus.
- Do not count payload compression while ignoring index metadata, alignment, conversion buffers, or navigation structures.
