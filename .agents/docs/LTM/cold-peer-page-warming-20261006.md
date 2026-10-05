# Cold Peer Restore and Page Warming, 2026-10-06

## Question and construction

The workload is shifou restoring a Qwen3-0.6B generation-2 session through a
separate `yesnod` Unix peer. The verified reader transfers 230,866,944 bytes
of exact BF16 attention state with 16 peer lanes, an 8 MiB arena, pipelined
parallel SHA-256 verification, and 16 shared Rayon workers. Flight and GPU
work are absent. The model-free probe connects before its timed barrier and
checks the generation, 56 attention records, and byte count after the read.

Three independent, non-reflink 442 MiB allocated database copies contain the
same checkpoint. Each arm calls `POSIX_FADV_DONTNEED` on its 1 GiB logical
`.yno` file before starting a fresh server. `mincore` then reports zero
resident pages out of 262,144, making the cold reset observable rather than
assumed. Each server's major-fault counter is sampled around one verified
client read. The one-off harness and raw JSON are under
`.agents-workspace/tmp/cold-prewarm-20261006/`; the original E2E checkpoint
and prior cold copies are under
`.agents-workspace/tmp/session-workflow-probe-20261005/e2e-bench-qwen-1980/`.

The first control read on each copy produced a `mincore` bitmap. Prewarm arms
use those bitmaps as an **oracle for a previously observed session footprint**.
This does not mean a new session can discover its pages without reading it,
nor that the offsets remain valid across file replacement or compaction.

## Measurements

All times below are three-copy medians in milliseconds. `Warm` is work done
before starting `yesnod`; `Read` is the verified foreground peer read. The
fault count is the increase in server major faults during that read. These
arms ran on a busy shared GB10 host, so compare the large within-experiment
effects and page/fault evidence, not small differences between arms.

| Prewarm arm | Warm | Read | Major faults | Resident before server |
| --- | ---: | ---: | ---: | ---: |
| Cold control | 0 | 535 | 953 | 0 MiB |
| `pread` entire 1 GiB file | 463 | 170 | 0 | 1,024 MiB |
| `pread` 238 MiB observed footprint | 318 | 166 | 0 | 239 MiB |
| `pread` largest 220.5 MiB data span only | 255 | 179 | 6 | 220.7 MiB |
| `pread` other observed pages only | 66 | 514 | 946 | 18.2 MiB |

The observed footprint has 60,970 resident 4 KiB pages ( 238.2 MiB ) in 118
contiguous runs. One run spans file offsets 229.938-450.438 MiB and accounts
for 220.5 MiB. The other 117 runs account for 17.7 MiB. Warming that one
data run alone nearly eliminates foreground major faults and cuts its read
time by roughly threefold; warming only the other runs does not. Thus the
storage-cold delay in this checkpoint is dominated by faulting its contiguous
payload span, not by index-page faults or socket round trips. The file layout
already gives this payload a useful contiguous span.

Advisory attempts were ineffective in this setup. `POSIX_FADV_WILLNEED`
across the file, followed by 50 ms, populated only 16 MiB and yielded a
571 ms read with 987 major faults. Repeated Linux `readahead(2)` calls over
the observed footprint populated about 22 MiB and yielded a 532 ms read
with 1,007 faults. Whole-file `readahead(2)` populated 32 MiB and yielded a
562 ms read with 1,027 faults. These calls returning does not establish that
the requested pages became resident; `mincore` is the guard that exposed it.

The prior cold-copy series measured 771, 836, and 781 ms first reads against
about 187 ms warm reads on the same checkpoint. The newer control series
measured 515, 553, and 535 ms cold reads. Host contention and timing changed
between series; the shared 953-major-fault delta and zero pre-read residency
show that both were materially cold. No production yesno or shifou source was
changed by this experiment.

## Implication

A foreground read can approach warm latency after a background worker has
actually populated the relevant payload pages. Whole-file warming moves more
work than necessary. A deployable targeted prefetch would need a versioned,
safe way for yesno to identify the physical spans backing a selected logical
query and invalidate that plan when storage changes. The oracle mask used
here supplies an upper bound on that opportunity, not such an API. Evaluate
upstream batched queries against the cold-fault baseline separately: fewer
query round trips do not by themselves prove that payload pages were
prefetched.
