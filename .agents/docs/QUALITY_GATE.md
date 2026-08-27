# Quality Gate

The checklist a change to `yesno-core` must pass before it is reported as done, plus the implementation conventions the checks encode. The `quality-gate` skill walks these sections in order and emits a verdict per check.

Sections are referenced as `QG §N` elsewhere in the docs.

---

## 1. Baseline Commands

`./scripts/gate.sh` runs all of these in order and reports a verdict per step; prefer it to running them by hand. `--deep` adds Valgrind and the sanitizers. `.github/workflows/ci.yml` runs the same set on every push, though it has not executed anywhere yet.

By hand, from the workspace root:

```bash
cargo fmt --all -- --check                                   # whole tree; baseline closed 2026-08-27
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
python3 scripts/check-layout.py    # ARCHITECTURE.md's layout diagram matches the tree
python3 scripts/check-r1.py        # no new Arrow types in yesno-core's public API
```

**`rustfmt <file>` does not format one file.** It follows every `mod x;` declaration, so `rustfmt yesno-core/src/lib.rs` rewrites seven files. On a clean tree that is harmless, but `cargo fmt` is still the right tool. `yesno-core/fuzz` is outside `[workspace] members`, so `cargo fmt --all` never sees it — format and check it separately.

- Clippy is `-D warnings`. An `#[allow(...)]` is acceptable only with a comment saying why the lint is wrong here — see the `wrong_self_convention` allow on `ChunkStreamExt::is_empty` for the shape of an acceptable one.
- `cargo test --workspace` runs unit tests, `yesno-core`'s nine integration suites, the satellite crates' suites, `yesno-e2e`'s Python scenarios, and doc tests. Do not filter it down for the final gate run, only while iterating. ( `-p yesno-core` is the right filter *while* iterating on core. )
- `cargo bench --bench setops` is **not** part of the gate. Benchmarks are a finding-generator, not a pass/fail criterion.
- **Running a subset of a gate is not running the gate, and a hand-assembled equivalent is current only until someone adds a step.** The list above is a convenience for iterating, not a substitute. On 2026-09-14 a session ran those commands all day and treated it as equivalent; `./scripts/gate.sh` then found two failures that set structurally could not -- a step-count mismatch, and a test in a crate the hand list never invoked, **already red before any of that day's changes**. Both surfaced in the first minute of running the whole script. This is the same failure this repository keeps recording in other forms: a tool and its documentation are two implementations of one check, and only one of them executes.
- **`verdict`'s step count cannot distinguish "the run stopped early" from "somebody added a step".** Both read as a red gate. If it fires, establish which before touching `EXPECT_STEPS` -- the tail of the output says whether the run reached the end. It exists because the gate once exited 0 having run less than half of itself.

### The external integration gates

`yesno-pg` and `yesno-mysql` are outside the Cargo workspace because each is
loaded into one exact database-server ABI. The root Cargo gate cannot build or
test either integration. Run the applicable independent gate:

```bash
./scripts/gate-pg.sh          # PostgreSQL 17 and 18 build, unit, SQL/isolation, drift, fmt
./scripts/gate-mysql.sh       # yesno-c, C++ unit tests, Arrow Flight, MySQL, plugin, fixture
./scripts/gate-search.sh      # Java helper plus pinned OpenSearch and Elasticsearch
```

Docker is the only host dependency for these three commands. They run in the
single all-in-one `yesno-e2e:local` image, which `scripts/build-e2e-image.sh`
builds for every containerized gate. Its build prepares the PostgreSQL and
MySQL Bazel outputs, JDK 21, Java/Gradle outputs, and checksum-pinned
OpenSearch and Elasticsearch archives, and it also carries what
`./scripts/gate-operator.sh` and `./scripts/gate-filesystems.sh` need.
Subsequent gates and sessions with an unchanged checkout reuse those layers
before running fresh fixtures.
`YESNO_E2E_IMAGE` overrides the shared tag; the per-integration names that
preceded it — `YESNO_BUILD_E2E_IMAGE`, `YESNO_DATABASE_BUILDER_IMAGE` and
`YESNO_E2E_DRIVER_IMAGE` — remain compatibility fallbacks.

**One image means one build.** Any gate's first run in a fresh checkout pays
for every integration's artifacts, not just its own. That is deliberate: the
alternative was three images that duplicated the source copy, the release-binary
build and the pinned toolchain, and had already drifted onto two different Rust
versions for the same `yesno-e2e` binary. The opt-in real-AWS gate is the one
exception and keeps a slim runner of its own, because its image is pushed to a
per-run ECR repository and pulled onto an EC2 host.

### Authority-sensitive infrastructure gates

The operator, native-filesystem, and real-AWS gates are opt-in because they
require Docker, KVM, Kubernetes, or billable cloud authority:

```bash
./scripts/gate-operator.sh
./scripts/gate-filesystems.sh
./scripts/gate-mount-propagation.sh
./scripts/gate-aws.sh
```

These gates are different evidence. Emulation checks AWS request and state
logic, the filesystem gate checks real kernel and privilege behavior, and the
AWS gate checks IAM, EBS, ECS, EKS, CSI, and live cleanup. A partial
`YESNO_AWS_ONLY` run must be reported as partial; separately green arms do not
prove that their shared Terraform stack and cleanup compose.

Cloud cleanup assertions must first observe at least one owned resource through
the same filter later required to reach zero. The observer deadline must exceed
the component timeout whose error it is waiting to collect. Capture resource
IDs, warning events, controller logs, source chains, and truncation notices
before cleanup removes the Job, claim, namespace, or Terraform evidence.

The PostgreSQL gate builds sha256-pinned PostgreSQL 17 and 18 servers from
source, initializes a throwaway cluster for each, installs the matching
extension, and diffs one checked corpus. The MySQL gate builds sha256-pinned
MySQL 8.4.0 plus pinned Arrow C++, OpenSSL, ncurses, patchelf, and Flight
dependencies, runs the native client's hermetic unit tests, links the
Bazel-built embedded and remote clients into
`ha_yesno.so`, initializes a throwaway server, and
runs the checked mysqltest fixture. Neither gate discovers a host database
installation.

**Which gate a change needs:**

| Changed | `gate.sh` | `gate-pg.sh` | `gate-mysql.sh` | `gate-search.sh` |
|---|---|---|---|---|
| `yesno-core` | yes | yes | yes | yes |
| `yesno-arrow`, `yesno-flight`, `yesno-wire` | yes | yes | no | yes |
| `yesno-c` | no | no | yes, plus `yesno-c/gate.sh` | no |
| `yesno-flight-c++` or `third_party/arrow` | no | no | yes | no |
| `yesno-pg` only | no | yes | no | no |
| `yesno-mysql` or `third_party/mysql` | no | no | yes | no |
| Java search helpers or plugins | no | no | no | yes |

A Rust change that satisfies Cargo can still break a Bazel dependency graph
or a server build. No gate above subsumes another.

**A SQL fixture that asserts the right rows does not prove PostgreSQL
pushdown happened.** Every pushdown and access-method fixture pairs its query
with an `EXPLAIN`, so the expected plan is part of the assertion. PostgreSQL
expected outputs are regenerated only through the explicit accept target; never
accept a diff without reading it.

### The standalone C gate

`yesno-c` is a separate Cargo workspace and is invisible to the root Cargo
gate. Any change to it or to `yesno-core` must also run:

```bash
./yesno-c/gate.sh
```

That gate runs test, Clippy, and format checks, then compiles the public header
and smoke client as strict C11, links the real static library, and exercises two
independent durable database handles. Build output stays under
`.agents-workspace/tmp`. The MySQL gate builds the same C boundary through
Bazel and does not replace this language-level gate.

---

### The Python client gate: `yesno-flight-python/gate.sh`

The installable Python package has its own language and cross-process boundary.
Run this gate for changes under `yesno-flight-python/` and for changes to the
Flight service, ticket, or expression wire format:

```bash
./yesno-flight-python/gate.sh  # lock, Ruff, mypy, real yesnod round trip, wheel
```

The script requires `uv`, builds the shipped `yesnod`, and points pytest at that
exact binary. A missing server is therefore a gate failure rather than a skipped
integration test. Its wheel is written under `.agents-workspace/tmp/`, not into
the source tree. This gate does not replace either Rust gate: a Flight or wire
change must run `gate.sh`, `gate-pg.sh`, and the Python client gate.

---

### The Java client gate: `yesno-flight-java/gradlew`

Run the checksum-pinned Gradle wrapper for changes under `yesno-flight-java/`
and for changes to the Flight service, ticket, expression, or view wire format:

```bash
cd yesno-flight-java
./gradlew build
```

The build compiles for Java 17 with `-Xlint:all -Werror`, runs the JUnit suite,
and fails on Javadoc errors. It complements rather than replaces the Cargo,
PostgreSQL, and Python gates.

---

### The Go client gate: `yesno-flight-go/gate.sh`

Run the standalone Go module gate for changes under `yesno-flight-go/` and for
changes to the Flight service, ticket, expression, view, stats, or leadership
metadata wire format:

```bash
./yesno-flight-go/gate.sh
```

The gate requires Go 1.25, checks formatting and module tidiness, runs `go vet`
and the race detector for both unit and integration builds, builds the exact
`yesnod` in the checkout, and exercises plaintext, bearer authorization,
leadership fencing, TLS, mutual TLS, streaming, and mutation paths against it.
A missing daemon or skipped live suite is a failure. This gate complements the
Cargo, PostgreSQL, Python, and Java gates; it does not replace them.

### Release workflow and multi-platform image

The release workflow must run the reusable CI gate before any image can be
published. Downstream jobs must distinguish a skipped prerequisite from a
failed one so pull-request path selection does not accidentally authorize
publication.

A successful multi-platform build is not sufficient evidence. Inspect the
`linux/amd64` and `linux/arm64` manifest entries independently and verify that
each contains binaries for its declared architecture. Keep cross-compilation
in `dist/build.Dockerfile` and assembly in `dist/Dockerfile`; the runtime stage
must not execute target-architecture code during the build.

Changes to `.github/workflows/release.yml`, `dist/`,
`scripts/build-release-image.sh`, or `scripts/check-image-binaries.py` must
preserve that ordering and architecture check. Local workflow and Dockerfile
validation cannot prove the remote GitHub job graph or registry manifest.

---

## 2. Invariant Conformance

Confirm the change preserves the invariants listed in `ARCHITECTURE.md` § Invariants. In particular:

- **No empty containers reach an `OrdSet`.** Container-level ops return `Option<Container>` with `None` for empty precisely so this cannot happen by accident. If you add a mutation path, check that it drops a chunk that has become empty.
- **`len` stays incremental.** `OrdSet::len()` and `Container::len()` are O(1) by contract. A change that recomputes cardinality by iterating is a regression even when it returns the right number — the cardinality identities in `ops::card` depend on cheap `len()`.
- **Run containers stay non-adjacent.** Two runs that touch must be merged into one. An adjacent pair is a valid *decode* input from some producers but never a valid in-memory state for us.
- **`Prefix48 < 1 << 48`.** Anything constructing a prefix from arithmetic rather than `split()` needs a check.

---

## 3. Test Layer Selection

A change must land coverage in the layer that could structurally catch its failure. Pick from the table; "a new unit test in the module" is rarely the right answer on its own.

| What you changed | Layer that must cover it |
|------------------|--------------------------|
| Container representation, promotion, demotion, `optimize` | `proptest_oracle.rs` — content preservation and invariants across mutation |
| A set-algebra kernel ( `ops/` ) | `proptest_oracle.rs` set-ops properties, plus `differential.rs` if the result shape can differ from `roaring` |
| Codec, `roaring_format`, or anything touching byte layout | `differential.rs` — byte-level identity in **both** directions |
| A `ChunkStream` operator or `Expr` | `expr_equivalence.rs` — lazy vs eager vs oracle |
| A `cardinality_dyn` override, or any "fast path" that avoids materializing | `allocation.rs` — an allocation budget, not a benchmark |
| `rank` / `select` / `min` / `max` | `proptest_oracle.rs` mutual-inverse property |

Additional rules:

- **A checked-in expected-output file is not an independent oracle — the diff review is, and it is asymmetric.** Replacing expected output with what the code printed makes the only assertion the one a human made while reading the diff. That read is good at "this line is wrong" and bad at **"a line that should be here is missing"** — and a missing row is what a lost write, a skipped key, or a dropped disjunct all look like. Do not let an expected-output fixture stand as the only coverage of a claim that fails by *omission*. Pair it with an assertion that names the expected value ( a `count(*)` that must be 1, a specific ordinal that must appear ), so the claim is in the file rather than only in the reviewer's attention.

- **An absence assertion cannot distinguish "correctly discarded" from "never there".** `BEGIN; INSERT …; ROLLBACK; SELECT count(*)` → 0 passes identically whether the buffer was rolled back or the write was never visible in the first place. Every such test needs a companion asserting **presence at the moment presence is claimed** — here, that the inserted row is visible *inside* the transaction. This is the same defect shape as the allocation test that measured nothing, arriving through polarity rather than through a refcount.

- **Tests derived from the implementation can only confirm the implementation.** Write the coverage list from the *specification* — the plan, the module `//!` block, the option's stated contract — before reading back what the code does. A property that lives in no single function ( a visibility model, a lifetime, an ordering ) gets no slot in a checklist organized by function, and is exactly what this step exists to catch.

- **A red→green transition from fixing a crash is not a verified test.** It has the same shape as sabotage-verification and satisfies the same instinct, while proving only that the code stopped aborting. If a fixture's first failure was a segfault, it has not yet been observed failing *for the reason it asserts*.

- **Bugs cluster where there is no independent oracle.** This crate's strongest layers each have one — `roaring`, `BTreeSet`, `num-bigint`, Python's `set`, the brute-force lowering oracle. When a new area has none, say so explicitly and treat every claim about it as unverified rather than as covered. ( Recorded 2026-08-29, after three defects in the table access method — the one area built with no oracle at all — survived a green gate and a completion report. )

- **Cross-session claims require independently connected sessions and deterministic coordination.** Transaction visibility, statement-scoped snapshots, blocking, cancellation, and cleanup cannot be proved by sequential commands on one connection. Use persistent sessions synchronized by explicit markers or advisory locks, with bounded waits and cleanup paths; do not use sleeps as the protocol.

- **New `cardinality_dyn` override ⇒ new test that a correctness test cannot replace.** The non-materializing walk is a parallel implementation of the materializing one; correctness tests cannot tell them apart because both return the right number.

**But an allocation count is not always the right instrument, and choosing it by habit has produced a test that measured nothing.** Containers are frozen ( `Container::freeze` ), so `Container::clone` is a refcount bump and allocates *nothing*. Anything of the form "is this cloning rather than lending" is therefore **invisible** to a counting allocator. Pick by the claim:

  | the claim | the instrument |
  | --- | --- |
  | does this scale per chunk | allocation count, compared across two sizes |
  | did the operator ask for a payload it did not need | a spy stream recording which method was called ( `stream::ops::counting_tests::Spy` ) |
  | is this the container's own memory | pointer equality against the source buffer |
  | does this finish at all | a worker thread with a deadline, so a regression **fails** rather than hangs |

- **Verify a new test against unfixed code before landing it, and re-verify old ones periodically.** Revert what the test pins; confirm it goes red. An instrument can lose its subject as the code improves — copy-on-write made pass-through allocation-free and silently blinded every allocation test watching for a clone, with nothing failing at the moment the coverage was lost. A retroactive sweep on 2026-08-26 found **one of twelve** measuring nothing, and the obvious repair to it did not work either.
- **Generators must stay boundary-biased.** If you add a strategy, make it cluster ordinals into few chunks or produce runs. A uniform `u64` strategy tests almost nothing in this crate.
- **Proptest regression seeds are a corpus.** `tests/*.proptest-regressions` is checked in. Add seeds, never delete them to go green.
- Never weaken an oracle, loosen a property, or raise an allocation budget to make a failing test pass.
- **A check whose error has nowhere to go is not a check, and can be worse than none.** Before adding a validation to a read or write path, confirm the caller can *carry* its failure. Adding the read-path checksum recomputation on 2026-09-14 made things strictly worse at first: `merged_chunks` returned a bare `Vec` and swallowed both failure paths, so a corrupt index node went from **silently correct** ( nothing checked it ) to **silently empty** ( something checked it and the error was dropped ). The check and the error channel have to land together.

---

## 4. Kernel Specialization Discipline

When adding a specialized (kind × kind × op) arm:

1. There must be a benchmark showing the generic kernel is the bottleneck for that arm. "Obviously faster" is not a reason.
2. `ops::generic` stays the oracle. The specialized arm must be differential-tested against it over boundary-biased inputs — not only against `BTreeSet`.
3. The generic path stays reachable and correct for that arm. Do not delete it.
4. Record the benchmark numbers, before and after, in `JOURNAL.md`.

### How to take a number, which is where most of them go wrong

These are rules rather than history. They were each paid for -- the incidents are
in `JOURNAL.md` under 2026-09-14 -- but what belongs here is the obligation, not
the anecdote.

- **A single timing is not a measurement, it is a sample.** Run at least three
  and compare **ranges**, not means. Run-to-run spread in this repository is
  **10-20%**, which is wide enough to invert a real 15% effect in either
  direction. On one day this flipped three separate conclusions: a scan that
  looked free cost 12%, a "17% win" was a 10% loss at a different shard count,
  and a rejected variant turned out to be the better one. Every one of them read
  the other way on a single run.
- **Print something whose right answer you already know, and look at it.** Three
  instrument defects in a day were each caught by a *derived* quantity that was
  impossible -- an implied CRC throughput of 218 GB/s against hardware's 10-25,
  an answer column reading 0 at every size, per-block timers not summing to the
  whole. None was caught by the headline number looking wrong, because it never
  did.
- **State the subject and assert it.** A probe that prints "8 KiB bitmap
  payloads" while its fixture builds 50 000 *consecutive* ordinals is measuring a
  six-byte run container. The header is not evidence; a computed check on the
  thing being measured is.
- **A constant derived through a defective path carries the authority of a
  measurement while being fabricated**, and nothing downstream can tell. Before
  a measurement sets a threshold, pin the paths under test against **each other**
  -- a cross-path agreement assertion validates the apparatus, where a
  behaviour test only validates the path that happens to work.
- **A measurement's *shape* decides what it can find, before any number is
  chosen.** Name the dimension the change touches and confirm the measurement
  **varies** it. A single-threaded run cannot see a concurrency defect; a
  phase-separated one cannot see an interleaving defect; a read-only one cannot
  see writer starvation; a one-shard one cannot see contention that scales with
  shard count. Each of those four was paid for on 2026-09-14 -- a global mutex
  added to the read path and measured only single-threaded; a read-scaling
  conclusion drawn with no writer present; a 22x commit finding measured at one
  shard where the bucketing stage could not scatter by construction. **The
  failure is not insufficient care.** In every case the cheap measurement was
  *structurally incapable* of seeing the defect, and ran clean.
- **A number must travel with what was held *fixed*, not only with how it was
  taken.** Naming the construction is not enough: a sweep that varies the wrong
  axis is a correct measurement of the wrong thing, and it reads exactly like a
  correct measurement of the right one. On 2026-09-14 a checkpoint cost was
  attributed to three `fsync`s from a sweep over **resident size** with dirty
  state pinned at four keys -- pinning the quantity that actually set the slope.
  The consumer's mirror-image error pinned the checkpoint interval. Both sweeps
  were competently run; both conclusions were wrong; neither recorded what was
  still. **Write down the held-fixed variables beside the varied one**, and ask
  which of them the mechanism could plausibly depend on.
- **The unit that needs repeating is the *conclusion*, not the timing.** Running
  each point three times inside a sweep is not the same as running the sweep
  three times, and the second is where a shape hides. A checkpoint-cost table
  taken one run per point on 2026-09-14 read as a smooth monotone rise; repeated,
  its bottom two points were **indistinguishable** and the trend existed only at
  the top. The consumer hit the mirror image the same day -- a repeat retired a
  published outlier ( 161 against a true 93-97 ) and a 409 ms worst case that
  came back 136 ms, both already in their README. **Their conclusion had been
  correct anyway**, which is what made it undetectable: an outlier that does not
  change the verdict leaves nothing for the verdict to flag.
- **A wrong inference that is cheap to falsify costs nearly nothing.** The
  discipline is not to infer less; it is to make falsification cheap enough that
  inferring is free. That is a property of how the experiment is set up, not of
  how careful the reasoning was.
- **Re-measure a ratio whose denominator may have moved.** A number that was
  correct when taken is not thereby current: this crate has recorded at least
  four ratios invalidated by the *other* side getting faster. Quote a figure only
  with the construction that produced it, or not at all.

---

## 5. Buffer and Zero-Copy Discipline

- R1 constrains the **stable public API**, not internal spelling. Container payload access still goes through `U16Store` / `BitStore`, while `scripts/check-r1.py` walks ancestor-module visibility so a public item inside a crate-private module is not misclassified as exported API.
- Mutating shared data copies. Do not "optimize" `to_mut` / `words_mut` into `Buffer::into_mutable()` — a container decoded from a page is a slice of a larger buffer, and that call would fail or, worse, mutate published bytes.
- After a change to the store layer, check that `Container` is still `'static + Clone + Send + Sync`. A lifetime leaking in here breaks every boxed stream downstream, and the error message will point somewhere else entirely.

---

## 6. Codec and Format Fidelity

- Payload bytes must stay byte-identical to the portable Roaring spec. `differential.rs` fails loudly if not, in both directions.
- `codec::decode` is a fuzz target by contract: any input returns `Err` or a valid container, and never panics. The `deserialize_never_panics` property must still hold after any change to it.
- Bounds, alignment, and cardinality checks belong in `decode` and produce a typed `CodecError`, not a panic, an `assert!`, or a silent clamp.
- The offset-header rule stays centralized in `roaring_format::has_offsets`. Do not re-derive it at a call site.
- We support CRoaring's `Roaring64Map` layout. If a change starts accepting Java's `Roaring64NavigableMap`, that is a scope decision for the user, not a bug fix.

---

## 7. Unsafe Code

- `#![deny(unsafe_op_in_unsafe_fn)]` stays.
- Prefer `bytemuck` to hand-written transmutes.
- A new `unsafe` block needs a `// SAFETY:` comment naming the invariant it relies on, a `JOURNAL.md` note, and a property test that would fail if the invariant were violated.
- An `unsafe` block that exists only to skip a bounds check needs the benchmark that motivated it.

### MIRI — withdrawn 2026-08-29

**MIRI is no longer a verification method for this project, and `./scripts/miri.sh` is no longer a gate.** It was removed from `scripts/gate.sh --deep` and from `.github/workflows/ci.yml`'s `deep` job on that date. The script is kept on disk and stays runnable by hand, because a *targeted* run is still sometimes the cheapest way to settle a question about one cast.

- **Why**: cost, and the failure mode cost produces. MIRI interprets, so the price is set by the **fixtures a test builds**, not by the kernel under test — and this suite's fixtures outgrew it. `ops::run::tests` exceeded 900 s; `matrix::seek` sat on a single test that builds 200 000-ordinal sets for **42 min 43 s** before being killed. A run that never ends never reports, which is the same hazard `gate-cannot-prove-it-ran` exists for.
- **What was lost**: cast checking under `-Zmiri-symbolic-alignment-check`. Valgrind does **not** replace it — Valgrind sees concrete addresses, so it passes whenever the allocator happened to align things.
- **Why that is survivable**: the crate's reinterpretations go through `bytemuck`'s *checked* entry points. `BitStore::try_words` is `bytemuck::try_cast_slice::<u8, u64>(..).ok()`, which **refuses** a misaligned slice at runtime instead of reinterpreting it, and every caller carries the `None` fallback ( it is mandatory, not defensive — an mmap-backed buffer legitimately is unaligned ). That protection is what makes this gate removable, so a hand-written transmute or an unchecked `cast_slice` now costs strictly more than it used to: there is no longer a tool that would catch it.
- **Do not re-add it to a gate on the strength of a fast targeted run.** What made it unaffordable is the proptests and the large fixtures; any selector broad enough to be worth gating on pulls them back in.

### Valgrind

Conditional: run it when you change `store/`, `db/`, or `checkpoint.rs`.

```bash
./scripts/valgrind.sh
```

- This is the **only** UB tool the project runs, and the only one that ever covered `store/segment.rs` — MIRI refused that file ( "Miri does not support file-backed memory mappings" ) even when it was gated, and both of the crate's `unsafe` blocks live there.
- A clean run does not verify extent reclamation. Logical reuse of a slot inside a still-mapped region is not a memcheck error, so the three-condition rule is checked by `tests/zero_copy_mvcc.rs` asserting on contents, not by this.

### Fuzzing

Also **conditional**: run it when you change `container::codec` or `roaring_format`. Both are parsers of untrusted bytes, and both have a stated never-panic contract that only a fuzzer really tests.

```bash
cd yesno-core
cargo +nightly fuzz run decode_container -- -max_total_time=300
cargo +nightly fuzz run roaring_import   -- -max_total_time=300
```

- `libfuzzer-sys` builds its bundled libFuzzer with `cc`; no `clang` is needed. It does need nightly.
- `fuzz/` is **outside the parent workspace** on purpose ( its `Cargo.toml` carries an empty `[workspace]` ), so routine `cargo test` never depends on nightly.
- The defect shape worth expecting is **structurally valid, semantically impossible** — a payload with the right length and an in-range cardinality that still describes a container that cannot exist. Length and truncation errors were always caught; every defect the first run found was of the former kind.
- Do not let a fuzz finding be fixed only in the target. A crash must end up pinned by a unit test in the module *and*, where a generator can reach it, by a property in `proptest_oracle.rs` — the fuzzer is not run in CI, so it cannot be the thing that guards the fix.
- A minimized crash input is worth keeping verbatim in a test ( see `the_fuzzer_oom_input_now_errs_cleanly` ). `fuzz/corpus` and `fuzz/artifacts` are gitignored and do not survive a clean checkout.

---

## 8. Documentation Conformance

- Module-level `//!` comments carry the *why*. If you change behaviour a `//!` block explains, update the block in the same change. A stale rationale is worse than none.
- Public items get a doc comment. Constants with non-obvious values get the reasoning, not just the value — `BITMAP_DEMOTE` is the model.
- Repo-authored docs use half-width parentheses and half-width colons, with a space before/after a parenthesis adjacent to non-whitespace.
- Work summaries go into an existing document under `.agents/docs/`, not `/tmp` and not a new stray file.

---

## 9. Journal and Backlog Hygiene

- Append a short entry to `.agents/docs/JOURNAL.md` for anything a future agent would want to know: a design decision, a bug class, a benchmark result, a rejected approach and why.
- Do not edit existing JOURNAL sections. Append.
- If the work leaves follow-ups, add them to `.agents/docs/TODO.md` rather than leaving a `// TODO` comment as the only record. Code TODOs are fine as pointers; they are not a backlog.
- If the work closes a `TODO.md` item, remove the item and note the outcome in `JOURNAL.md`.

---

## 10. Known Non-Blocking Issues

Track things that are deliberately unresolved here so a gate run does not rediscover them as new findings each time.

- **`cargo fmt --check` passes on the whole tree**, as of 2026-08-27. The baseline — 65 hunks at close, 89 when first measured on 2026-08-25, all line-wrapping — was cleared by a one-shot `cargo fmt --all` plus a separate pass over `yesno-core/fuzz`, which sits outside `[workspace] members` and which `cargo fmt --all` therefore never reaches. `scripts/gate.sh` checks both. The consequence worth keeping: a bulk reformat is now a **no-op**, so any diff `cargo fmt` produces is yours and belongs in your change. ( Historical note, since it recurs: earlier figures of 330 and "192 across 35 files" were artefacts of passing every `.rs` file to rustfmt at once, which counts each child once per ancestor that reaches it. )
- Java's `Roaring64NavigableMap` is unsupported by design.
