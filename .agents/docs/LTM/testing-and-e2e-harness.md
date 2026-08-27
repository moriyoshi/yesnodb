# Testing and the End-to-End Harness

## Summary

The test suite is layered because semantic correctness, byte identity, lazy equivalence, resource behavior, persistence, concurrency, and operational sequencing fail in ways the other layers cannot observe. A recurring project lesson is that a green test is not evidence until the fixture can reach its subject and the assertion discriminates the fixed path from the broken one.

## Key Facts

- `proptest_oracle.rs` uses a boundary-biased `BTreeSet<u64>` oracle.
- `differential.rs` checks semantics and portable-Roaring byte identity.
- `expr_equivalence.rs` checks lazy versus eager evaluation and cardinality.
- `allocation.rs` guards resource behavior invisible to value equality.
- `stream_conformance.rs` observes production order and non-empty fibers.
- Durability, crash, invariant, concurrency, and zero-copy suites each own distinct storage properties.
- E2E scenarios use Python's `set` as the operational oracle.
- A scenario must call a host verb and contain an assertion; every advertised verb must have a scenario caller.
- Host timing loops belong in `q_time`; Python loops measure monty overhead.
- Accept-generated expected output is not an independent oracle; the human diff review is the oracle and is weakest at missing rows.
- Cross-session isolation properties need a synchronized two-session harness, not sleeps or a one-session SQL script.
- Network scenarios may block inside a host-owned Tokio runtime; they do not need asynchronous Python interpreter support for sequential operations.
- Integrations that require external runtimes use explicit process lanes rather than granting ambient filesystem or network authority to Monty scenarios.
- A cleanup assertion against a resource filter needs a positive observation while the resource exists; zero after cleanup is otherwise vacuous.
- An observer's deadline must outlast the component it observes, or the harness replaces the component's useful error with its own timeout.
- Diagnostics must run before cleanup destroys their evidence and must disclose truncation rather than returning plausible partial answers.
- A passing sabotage can mean either a weak injected fault or a weak assertion; compare the healthy and sabotaged observables before deciding which.
- Tests for multi-dimensional behavior must cross the dimensions in one fixture. Separate width round trips and narrow-width search tests left wide `LeafRef::search` completely uncovered.
- Before measuring a fast path, force it and an independent path to agree on the answer; a defect in the measured path otherwise gives a false threshold the authority of a benchmark.
- **A sabotage must be semantically wrong and structurally identical.** If it perturbs what the compiler sees, the compiler answers first and the test is still unmeasured.
- **A fix with two call sites needs two sabotage-checked tests**, and the partial guard is indistinguishable from a whole one: the suite is green either way and the passing count goes up.
- **A test's power is a measurement, not a property of its shape.** Sabotage-verification says whether a test has power; only a calibration table says where the cliff is, and a later shrink then has to cross it knowingly.
- **A calibration is a property of a fixture, not of a bug.** The variable that makes one fixture blind is not the variable in another.
- A test asserting the *absence* of an interaction has no line whose removal reddens it. Its guard is pairing with a test that proves the machinery works.
- Point a branch or refusal audit at code that **reports or refuses** rather than at kernels: a suite built around answers being correct covers answer-producing paths exhaustively and refusal paths only if somebody thinks of them.
- A test that only asserts a rejection passes equally against a function that rejects everything. Assert the largest accepted value beside the smallest rejected one.
- **The accepted neighbour must be *used*, not merely constructed.** A consumer's bound test paired a refusal with its accepted neighbour and still missed a divide-by-zero, because the neighbour was only created and never written to. Pairing is the right discipline and is satisfied by a value that cannot be used.
- **A bound guards the arithmetic at its own call site, and the binding constraint often has none there.** Two bounds shipped in one week were both wrong that way: a doc-id bound guarded the forward row's address and missed that block statistics are keyed by block number, capping ids four million times lower; a dimension bound guarded the key's index field and missed that `rows_per_block` is integer division, so every width above 65 536 was creatable and divided by zero on first use. Ask what *else* consumes the value, not only what the guarded expression computes.

## Details

### Reachable generators

Uniform random `u64` values almost never fill an array container, form useful runs, or reach the ordinal ceiling. Generators deliberately bias cardinality, representation, prefix layout, range width, and high ordinals.

The specialized-kernel property initially produced almost no bitmap operands because the supposedly dense generator deduplicated below `ARRAY_MAX`. The fix sized the unique result explicitly above the boundary and asserted every kind pair occurred often enough.

History-sensitive states require history-sensitive generators. Serialization after removal from a promoted bitmap caught a format bug insertion-only generators could not represent.

### Vacuity and sabotage

Several tests passed while unable to see their subject:

- an allocation counter bracketed only `len()` after mask construction;
- a pointer-identity claim used an allocation budget loose enough to absorb a copy;
- a DataFusion oracle made the unindexed disjunct false for every row;
- an E2E test reopened before reading, removing the live memtable/store conflict it was meant to test;
- a scenario implemented a research algorithm in Python and asserted properties of that implementation rather than of yesno.

Two complementary assertions are sometimes needed where one reads as sufficient. A source that could not distinguish an absent key from a failed read needed both "an unknown term is an empty table, not an error" and "a failed read is an error, not an empty table": the broken behaviour satisfies either one alone, and only the pair pins the distinction. The reason that gap survived is worth keeping too -- the only implementation of that trait for most of its life *could not fail*, so no test written against it could have told the two cases apart, and the missing case was invisible rather than untested.

The standing check is: does the broken implementation make this test fail in the way its assertion is phrased? Sabotage is strong evidence, but not sufficient when coupling to the broken code is incidental.

**A sabotage must also be structurally identical to the real code.** Replacing a C dispatch adapter with the sequential default left `user_data` unused and `CDispatch` never-constructed, so the gate exited **101** in clippy before a single assertion ran -- a sabotage that stops the build is not evidence the test catches anything. The version that proved the assertion passed a null callback: it compiles clean and changes only the behaviour under test. The same shape appeared as a reversed `task -> s` permutation that correctly changed nothing.

A sabotage is calibrated to the mechanism, not merely to the function. Reporting every other prefix from `ChunkSource::occupancy` left the 256-bucket summary unchanged, so the segmented-source oracle still passed: weak injection, sound test. Suppressing segmentation entirely also passed the first allocation guard because its floor of 800 was below both the healthy 1,038 and broken 910 counts: sound injection, weak test. The repaired floor is 970, between the two observables.

The wide-leaf regression needed the same two-stage correction. A search fixture varying only low 48-bit key differences triggered the suffix-slice panic but could not expose truncation to 64 bits. Adding keys that differ above bit 64 makes the silent half fail with the wrong insertion point. Public reachability additionally requires reopen and four ordinals per key so the lookup traverses the persisted tree and an out-of-line array payload.

Agreement tests are especially valuable before deriving a performance constant. The batch-order test compares key-major and descending document-major inputs, with insert then remove of the same ordinal so an unstable same-key order changes the answer. The consumer's planner threshold likewise became trustworthy only after forced fast and slow paths agreed; all prior fixtures fit inside one forward block and could not see the targeted-read defect.

### Calibrating a churn fixture, and why the corpus convention nearly produced a blind one

`e2e/scenarios/slab_reuse.py` is the gate-level regression for the allocator's stale bump pointer: it rewrites every key every round at a cardinality walking a ladder of size classes, so extents are superseded, reclaimed, their slabs emptied, and the class asking for a free slab next is usually a different one. It runs under `yesno-e2e/tests/scenarios.rs::every_scenario_passes`, **verified by running it and seeing the file named in the output** rather than assumed from the directory it sits in.

A new fixture was justified rather than assumed: with the fix reverted, `integrity.py`, `lifecycle.py`, `durability.py`, `mvcc.py`, `ckpt_under_reader.py` and `aged_state.py` all pass. `integrity.py` churns and calls `fsck`, which is why it looked like the natural place to extend -- it is blind anyway.

Three separate blindnesses had to be cleared, and only one was the corpus size:

- **The detector.** The first sweep reported every scenario as `CAUGHT` while the output plainly read `ok`, because it grepped text for "fail|error|assert" instead of reading the exit code.
- **The fixture's ladder.** Cardinalities of 12 to 900 ordinals encode to at most ~1800 bytes, under `PACK_MAX` ( 2028 ), so every chunk went into a shared packed page -- one class for everything and no migration at all. The scenario now **asserts its own ladder exceeds `PACK_MAX` before it starts**, because a fixture-parameter error is what a parameter check catches.
- **An inherited calibration.** The table copied in from a consumer's workload made **document count** the variable. Measured here it is `rounds`:

```text
  keys   rounds   against the reversal
     2        4   passes -- BLIND
     8        4   passes -- BLIND
     2        6   catches it
    64        6   catches it
```

Detection needs one full pass of the six-entry ladder so a class is actually left behind, plus `RECLAIM_CKPT_DELAY` checkpoints before a superseded extent is reclaimed. **Two keys suffice; four rounds do not, at any width.** The consumer's workload needed volume to make slabs empty; this one empties them by construction and needs *time*. Both tables live in the scenario's header with the reason they differ, so the next person does not shrink the wrong dimension. Defaults are 32 x 12, twice the measured threshold, at about 5 s.

**The `e2e/scenarios/` convention of running at a small corpus so the gate stays quick would have produced a blind fixture here**, and a blind fixture looks exactly like a working one. Any corpus chosen for a churn fixture has to be calibrated against the reversal, with the table kept in the fixture.

### Auditing refusal paths and branches nothing reaches

A counter on a branch answers "does anything reach this", and the answer has **four** outcomes of which only the first is work:

1. **Untested** -- reachable, ours, nothing exercises it. `CodecError::MisPointedExtent` ( the diagnostic written for the week's most serious bug, never produced by any test ), `BadRunCount` in the fuzz-contract decoder, `build_updating`'s strict-ascend refusal ( an unsorted delta would not error, it would emit a tree with entries out of order that every later lookup misses ), `Allocator::alloc`'s unknown-class refusal, and `build_updating`'s reuse-after-decode branch, reachable only by deleting a key that was never written, positioned inside an existing leaf.
2. **Unreachable by construction** -- `empty_leaf_skip` guards a leaf `pack_leaf` never emits; `no suffix width admits this key` can only fire if a single key does not fit at width 14, which is `CHUNKKEY_BYTES`. A test here would assert what the code forbids, and writing one would have looked exactly like diligence.
3. **Not provokable on this host** -- `UnsupportedEndianness`, gated on `cfg!( target_endian = "big" )`.
4. **No producer at all** -- `UnknownKind( u8 )`, whose only mentions anywhere are its declaration and a classifying match arm. That is an API question for the maintainer, not a testing one, because it is a variant of a public enum re-exported at the crate root.

**The sweep produces the arithmetic and distinguishes none of them**; the classification is judgement.

An **error enum is a bounded enumerable set**, and `Drop` is the one point every instance passes through, so a temporary `impl Drop` tallying variants answers "which of these does this workspace construct" exactly, with no hand-placed counters. Its limit is worth knowing before relying on it again: `Drop` forbids moving out of the type, so the moment a test destructures a variant **by value** the probe stops compiling. It survives today only because `AlreadyOpen( String )` is the one non-`Copy` payload and every match on it binds a wildcard -- a property of current test style, not of the design.

### Independent oracles and omission failures

A green gate concealed wrong table-AM reads, lost write ordering, and absent snapshot pinning because fixtures, comments, and expected files all descended from the same implementation intent. Strong layers use an oracle with a different ancestor: `BTreeSet`, `roaring`, `num-bigint`, Python `set`, brute-force lowering, or an unoptimized SQL plan. Correlated checks multiply confidence in one premise rather than challenging it.

Expected files created by an accept target are useful regression artifacts after review, but their code-generated values cannot independently detect omitted rows. Index-AM, FDW, join, aggregate, and write paths therefore gained differential oracles whose sabotages remove the optimized arm or compare against an independently enumerated result.

The PostgreSQL harness supports two persistent sessions, marker-synchronized asynchronous commands, and advisory-lock phase control. This is what makes transaction and statement snapshot claims observable without timing guesses.

Some obligations need a stronger mechanism than a fixture. Checkpoint adoption before durability changes no on-disk byte until a later sync fails, so a one-fault test cannot observe it. `checkpoint::run` returns a `Durable` token required by `adopt_superblock`, making the invalid order unrepresentable. Tests still own the independently observable half: readers must return complete oracle sets while checkpoint syncs run without the store lock.

The full gate is also a test surface rather than a list of commands. A hand-assembled approximation omitted `yesno-e2e` library tests and could not exercise the gate's executed-step counter. Adding two checks without raising the expected count correctly failed as `ran 14 of 12 steps`; only the actual script could distinguish that integration error from individually green commands.

### E2E design

Scenarios are small Python programs under `e2e/scenarios/`. Host verbs are prefixed `db_`, `snap_`, `set_`, `q_`, or `st_` so they do not collide with Python builtins such as `open`, `min`, `max`, or `len`.

Handle spaces are tagged. Separate vectors starting at zero are not disjoint namespaces: a set handle and query handle can otherwise share the same integer and a transposed argument returns a plausible wrong answer.

Measurement fixtures live as scenarios when their subject exists in production and their assertions can be phrased against yesno. Research prototypes for algorithms absent from `src/` live under `.agents-workspace/tmp/prototypes/` and are disposable by design.

PostgreSQL, MySQL, and search-engine integrations are opt-in scenarios on the
same runner. PostgreSQL and MySQL link one featureless fixture host and compose
the same generic `fx_*` resource, process, readiness, session, transcript, and
cleanup utilities; backend setup remains scenario data, with no backend Rust
world, feature, entrypoint, or helper binary. Search scenarios compose ordinary
database and Flight verbs with `search_*`; host-side search verbs invoke the
Java helper, download SHA-512-pinned engine archives, and own disposable plugin
processes. Python maps key 42 to ordinals 1, 3 and 5 and key 91 to 3 and 4, then
requires the real engine queries to return those exact IDs. A thin shell
selector must not grow a second runner or fixture lifecycle.

### Resource observables

Allocation counts remain necessary for detecting per-chunk materialization and missing shared buffers. They are not universal. Frozen containers make clone waste allocation-free, so spy streams record whether an operator asked for a payload or only a cardinality. Pointer equality is the correct test for shared-object identity.

### Generic processes and PTY transcripts

External databases use one featureless `fixture_host` and shared `fx_*` primitives for declared resources, paths, files, readiness, processes, sessions, transcripts, and cleanup. PostgreSQL and MySQL identity remains in Python. This keeps backend lifecycle out of Rust feature flags and prevents a second runner from drifting away from Monty's verb coverage audits.

`fx_start_tty` keeps stdin in a harness-owned pipe while joining stdout and stderr on a PTY. This is required for clients such as `psql` whose tuple output block-buffers behind a pipe and can cross a marker. The narrow `openpty` ownership bridge has a property that sends arbitrary printable lines through many descriptor pairs and observes terminal newline translation.

### Opt-in infrastructure gates

Scenarios requiring Docker, KVM, engine downloads, Kubernetes, or cloud resources remain outside `e2e/scenarios/`, so the routine Cargo gate acquires none of those host authorities. Thin gate scripts select paths and timeouts; they do not own alternate lifecycle logic.

The all-in-one filesystem image directly boots a kernel under QEMU/KVM and drives it over a framed serial console. Real ZFS, Btrfs, and LVM scenarios observe provider creation, direct-path negotiation, streamed backup, archive/restore, release, and crash reconciliation. Serial orchestration avoids depending on guest SSH and preserves failure transcripts.

Winterbaume exercises production AWS SDK request serialization and state transitions but cannot create a block device, mount a filesystem, or supply IAM. Terraform owns the disposable real-AWS acceptance boundary. A local emulator pass and a live-cloud pass are different evidence and must be reported separately.

Authority-sensitive tests inspect runtime facts from `/proc`, mountinfo, provider objects, and exact results rather than rereading harness configuration. A least-privilege claim is vacuous when the gate starts the daemon as root.

The real-AWS gate keeps Terraform responsible for infrastructure and moves imperative orchestration into `e2e/aws/gate.py` over small `cloud_*` intrinsics. This preserves one scenario vocabulary and makes each remote provision, bootstrap, and workload step independently reportable. Every cloud verb refuses to act unless `YESNO_AWS_GATE=1`, so the ordinary verb-dispatch test cannot spend money.

The gate has four arms: local EBS, deferred ECS, deferred EKS, and the operator on EKS. `YESNO_AWS_EKS=0` omits cluster resources while defaulting to coverage, and `YESNO_AWS_ONLY` selects one arm but rejects unknown or contradictory values so a typo cannot run nothing and pass. Local plus ECS passed together, deferred EKS passed independently, and the operator arm passed independently; those partial runs prove each path but not their interaction in one full run.

Live runs established several harness rules. A filter used to assert resource absence must first match a live resource. Progress belongs on stderr when the scenario's stdout is captured. Curl retry loops need per-attempt connection and overall timeouts, not only a bounded attempt count. The observed component's timeout must fire before the harness wait. Cluster dumps must happen inside the failing arm's cleanup trap, before Jobs, claims, events, and namespaces disappear. Warning events and controller logs need reserved output budgets; splitting JSON on commas silently destroys comma-rich SDK errors.

Recovery tooling is part of the gate. `gate-aws-destroy.sh` parses Terraform state structurally, treats an unreadable state as potentially live, refuses to destroy runs whose argv or environment is visible under `/proc`, and can sweep only unattached, gate-tagged orphan volumes. `YESNO_AWS_KEEP=1` reaches the remote cleanup traps as well as Terraform teardown; stopping only the final destroy would retain a billing cluster after deleting the evidence it was meant to preserve.

Process-wide instruments need process isolation. The replication read-amplification test reads `/proc/self/io`, so moving it into a one-test integration target made the exact counter private to its subject. Taking a minimum over repeated attempts did not remove continuous interference from neighbouring tests.

## Files

- `yesno-core/tests/` - property, differential, allocation, concurrency, durability, invariant, and conformance suites.
- `scripts/gate.sh` - the executable composition and completeness check that hand-assembled subsets cannot reproduce.
- `e2e/scenarios/` - operational and measurement scenarios.
- `yesno-e2e/src/` - host verb dispatch and typed handle tables.
- `yesno-e2e/src/search.rs` - shared `search_*` host verbs and pinned downloader.
- `e2e/{postgresql,mysql}/` - ABI-pinned external database scenarios and exact fixture corpora.
- `e2e/search/` - Java helper, OpenSearch, and Elasticsearch scenarios.
- `e2e/operator/` - Docker-only Kubernetes scenario on the common runner.
- `e2e/filesystems/` - KVM-backed ZFS, Btrfs, LVM, archive, and propagation scenarios.
- `e2e/aws/` - Terraform-owned live EBS acceptance scenario.
- `scripts/{gate-aws.sh,gate-aws-destroy.sh,check-runner-scripts.py}` - billable gate entry, recovery path, and static remote-script syntax check.
- `scripts/gate-search.sh` - scenario selector for the ordinary runner.
- `scripts/gate-{operator,filesystems}.sh` - opt-in infrastructure selectors.
- `.agents/docs/TESTING.md` - canonical harness and fixture guidance.

## Test Coverage

Run one E2E scenario with:

```text
cargo run -p yesno-e2e -- e2e/scenarios/<name>.py
```

Use `--show-output` for passing measurement tables and `--arg name=value` for full-scale corpora. The workspace gate runs the small asserted corpus.

## Pitfalls

- Never weaken an oracle or raise an allocation budget merely to make a regression green.
- Do not infer that a passing sabotage indicts the test until the injected fault is shown to survive intermediate summaries.
- Do not choose an allocation threshold without measuring both the healthy and sabotaged counts it must separate.
- A timeout guard must cover warm-up and measured work.
- An empty or non-empty search result needs a positive control before it is treated as evidence.
- Do not treat an accepted expected-output file as an oracle for missing rows.
- Do not claim cross-session isolation from a single-session fixture.
- Do not put a Python reimplementation of an unshipped algorithm in `scenarios/` and call it a yesno test.
- Do not let a harness timeout expire before the timeout whose diagnostic it is waiting to collect.
- Do not collect post-mortem state after the cleanup that deletes it.
- Do not summarize identifiers, status, or error tails into a count or truncated fragment when the full evidence is already available.
