# Measurement and Investigation Methodology

## Summary

Performance and correctness investigations in this repository are designed to discriminate between competing explanations, not merely to produce a passing number. A useful experiment defines its denominator, controls corpus construction, checks the expected sign and order of magnitude, includes a positive control where possible, and records enough provenance to reproduce the result. Instrumentation is treated as part of the system under test because it can change or even eliminate the behavior being measured.

## Key Facts

- Before accepting a measurement, check its sign, order of magnitude, and whether the expected-correct value was calculated independently.
- Comparisons need the same setup path and corpus. Similar-looking fixtures can encode different distributions, sizes, or lifecycle states.
- A positive control should demonstrate that the experiment would detect the defect or cost it claims to guard against.
- A sabotage that does not compile is not a positive control; the modified program must run and the intended check must fail.
- A ratio is not a result until both of its terms have been checked for movement. Two August 2026 investigations produced improving ratios whose numerators were unchanged: a trie's advantage grew with chunk width only because the array baseline it was measured against degraded, and a SIMD kernel's speedup grew with cardinality only because the scalar baseline degraded as branch history outgrew the predictor.
- A benchmark that reuses one input pair measures a machine that has already learned the answer. `intersect_crossover` understated the scalar array merge by **4.4x** for this reason. The control is a fixture with identical layout, footprint, and stride that varies only the values.
- Distinguish the mechanism when reusing operands. Branch-history memorization affects data-dependent loops; a branchless word loop is instead flattered by staying cache-resident. Both are real, and which one applies must be established rather than assumed.
- A whole-text `contains` assertion over rendered output is vacuous whenever any two rendered quantities can coincide. Scope positive assertions to the specific line, and add negative assertions that the unconverted value appears nowhere.
- Redundancy between reported columns defeats presence-checking. If a derived column is by construction equal to one of its inputs on the test corpus, choose a corpus where every reported total differs.
- An extrapolation model must be validated against the instrument at the one point where both exist. An occupancy model using sampling with replacement underestimated trie nodes by 13.7% at half density while looking correct in every sparse row.
- A characterization test may intentionally pin known-bad behavior; name the defect explicitly so a later fix has a clear red-to-green transition.
- Adjacent-size ratios often reveal hidden fixed costs, thresholds, or branch changes more clearly than a single absolute timing.
- Every percentage or ratio needs an explicit denominator; otherwise economically different observations can look equivalent.
- Benchmarks should exercise the reachable public or operational path. Timing an isolated helper can omit conversion, planning, I/O, or interpreter overhead.
- Nanosecond measurements in end-to-end scenarios must keep the repetition loop in the Rust host via `q_time`; a Python loop primarily measures `monty` calls.
- Work performed by Python-side walks should be reported with counters rather than wall-clock columns.
- Instrumentation can perturb ownership, allocation, optimization, and scheduling. If the observed subject disappears under instrumentation, the measurement is invalid rather than reassuring.
- An unwired parameter sweep is still useful only when a calibrated positive control proves the sweep and reporter can expose a real difference.
- A sweep's own guards need a control run with each guard removed; one documented guard was found to change nothing because it cited a symbol outside the sweep's declaration pattern.
- Classify a caller as test or production by syntax rather than file path: a unit-test module inside a production file otherwise reads as a production caller, which understated one sweep's test-only set more than fourfold and silently.
- Prefer re-running a cheap sweep to caching its verdicts; a verdict list ages in the direction that looks safe, and nine of fifteen expired within two weeks.
- Historical benchmark values need provenance. Measurements from an uncommitted or changing tree should be labeled accordingly and not treated as a durable baseline.
- Stale measurements are findings to refresh, not facts to preserve indefinitely.
- Test fixture construction is part of the test. A fixture that never reaches the intended representation cannot validate the targeted path.
- A benchmark must be re-justified after its implementation changes; a previously harmless reuse pattern may become cache- or predictor-dominated.
- Reference operands must use the intended representation. Comparing a run kernel with a reference bitmap measures representation choice, not the kernel.
- Demand surveys should compare a proposed abstraction in each consumer-specific unit and name the incumbent it must beat.
- A diagnostic that returns a plausible partial answer is more dangerous than explicit silence; preserve identifiers, truncation notices, source chains, and the moment before cleanup.
- Exact process-wide counters are not private measurements. Isolate the subject in its own test process instead of filtering continuous interference statistically.
- A single timing on a loaded machine is an observation, not a stable scenario cost; repeat it before using it to choose suite structure.
- When a TODO says "define X first", identify the missing operator-facing unit before assuming the blocker is an unresolved algorithm or policy.
- A single timing is a sample. Repeat the conclusion across batches and across every swept point, and record what was held fixed as part of the measurement frame.
- Print a derived quantity whose plausible range is already known. An impossible answer can expose a broken fixture even when the headline ratio looks convincing.
- Attach the evaluation frame to every imported number. Per block, per operand, per query, and per container are different quantities even when they share a noun.
- A baseline can flatter a change, while an unrelated bottleneck can hide it. Neither direction is detectable from the ratio alone.
- A process poll must exclude the command doing the polling; `pgrep -f` on a string present in its own command line is permanently nonzero.
- An uncommitted upstream tree used through a downstream path dependency is a shared runtime. Temporary logging and instrumentation are visible to the consumer.

## Details

A good investigation begins with competing hypotheses and chooses an observation that separates them. The outcome may be a timing, allocation count, serialized byte count, state transition, or invariant check, but it must be coupled to the mechanism under study. A green result without sensitivity evidence is weaker than a failing experiment that clearly identifies why the hypothesis was wrong.

Performance work in particular needs layered controls. The corpus generator should be checked independently, the benchmark should demonstrate that it reaches the intended API and representation, and the measurement harness should be sabotaged or calibrated once to show that it detects a known regression. Results should distinguish latency from work counters and total storage from one component's bytes.

Investigations also have a documentation lifecycle. Record the command, relevant configuration, code state, corpus shape, units, and interpretation in the journal. Promote durable lessons into LTM, while leaving provisional numbers and abandoned hypotheses in the append-only source history.

When retiring a design document, first move every surviving conclusion to the action item or canonical document it constrains. A to-do that merely points at a disposable derivation is fragile; a to-do that contains the conclusion can survive removal of a superseded design.

A benchmark can remain stable while ceasing to measure its named comparison. The matrix solve group became implementation-versus-itself after `solve_gf2` began delegating to LU; only re-deriving both arms exposed it. Names must describe the code that runs now, not the historical question that created the group.

Performance conclusions must preserve representation controls. The apparent run x run deficit vanished when the reference was actually run-optimized. Conversely, the real space-versus-time tension remained: a legal high-run container can save bytes while costing much more to intersect than a bitmap.

Negative product research is durable when it records demand, incumbent, and units. The ternary-lens survey rejected most consumers because their natural block structure, algorithms, or established encodings already dominated the proposal. One plausible unreplicated consumer remained; that is a scoped research lead, not authorization to ship a new public module.

Live cloud investigation repeatedly showed that each layer can report a truthful but non-actionable summary of the layer below it: a scenario timeout, an active Job, a scheduler failure, and finally the CSI provisioner's EC2 error. Output caps, cleanup order, namespace selection, and JSON filtering are therefore part of diagnostic correctness. A count should not replace resource IDs already in hand, normal events should not crowd warnings out of a fixed budget, and comma-splitting JSON must not discard the tail of an escaped error value.

An observer must outlast the component whose diagnosis it wants. The AWS harness once waited 900 seconds while the archiver's own materializer deadline was 3,600 seconds, guaranteeing that the harness would replace the useful error with `still running`. Retry-loop bounds likewise multiply attempts by the worst per-attempt cost; a curl without connection and overall timeouts makes a nominal five-minute loop effectively unbounded.

Exact instruments still need exclusive scope. `/proc/self/io` reports process-wide bytes, so several attempts all remained contaminated while neighbouring Rust tests continuously read similarly sized files. Moving the measurement into a one-test integration target removed the interference; taking the minimum did not. This is a structural fix, not a statistical one.

## Instrument failures and the controls that exposed them

Four apparatus defects on 2026-09-14 produced plausible headline results:

| Subject | Apparatus defect | Control that exposed it |
|---|---|---|
| Planner cost | `format!( \"{p:?}\" )` measured the debug formatter, while disjoint operands made execution return zero | print the answer cardinality |
| Checksum cache | consecutive ordinals optimized into a six-byte run while the probe claimed to hash 8 KiB bitmaps | implied throughput was an impossible 218.7 GB/s |
| Carry-save comparison | a word-major ripple baseline blocked vectorization and flattered carry-save | a supposedly allocation-heavy arm beat the baseline |
| ASan monitoring | `pgrep -f 'sanitizer=address'` matched its own polling shell | compare log modification time with wall clock |

The control should be a quantity whose answer is known for reasons independent of the implementation being measured. The planner probe's answer column, the checksum probe's bytes-per-second line, and the carry-save loop-order comparison all made a pleasant ratio subordinate to an impossible fact.

### Repeat conclusions, not only timings

Repeating three timings inside one configuration is insufficient when the claim comes from a sweep. A checkpoint table initially showed a smooth 16.7, 17.9, 20.2, 25.2, and 40.8 ms rise from one sample per point. Repeating every point showed 2,000 and 200,000 dirty ordinals overlapping completely, with separation only at 4,000,000. The durable conclusion is a roughly 17 ms floor plus a strongly sublinear super-floor term, not a proportional slope.

The same rule retired outliers that left their verbal conclusion unchanged. An outlier that does not flip the verdict is invisible to verdict review, so the sweep itself must be repeated even when every inner timing loop already takes several samples.

State what was held fixed. Sweeping resident size while dirty state stayed at four keys could identify a fixed checkpoint floor but was structurally unable to measure dirty-work growth. A read-scaling test with no writer could measure reader contention but could not expose checkpoint starvation under a candidate `RwLock`. The shape of the experiment decides which defect is findable before any number exists.

### Frames, baselines, and masked effects

Three confident consumer-size statements failed because their frames changed in transit: one chunk inside a per-block cursor is not one expression operand, a 122 MB logical posting list is not 122 MB copied by a zero-copy reader, and an O(chunks) reopen cost was an implementation artifact removed by sharing an immutable plan. Record the unit together with the boundary over which it was observed.

A comparison can be wrong in both directions. A naive word-major ripple baseline made carry-save look 3.6x to 5.5x faster and aligned with the proposal's inflated model; the corrected level-major baseline reduced that to 1.09x to 1.52x. Later, an end-to-end profile showed no benefit because posting-list reads dominated. Removing that bottleneck exposed a 1.34x gain consistent with the microbenchmark. One baseline flattered the change and one surrounding system hid it.

### Sabotage and unobservable obligations

A passing sabotage needs diagnosis. Dropping every other source prefix was absorbed by 256-bucket occupancy summarization, so the injection was too weak. Suppressing segmentation also passed an allocation floor of 800 because healthy and broken counts were 1,038 and 910, so the assertion was too weak. Only measuring both sides distinguishes the two cases.

Some obligations require a type rather than a test. Adopting a checkpoint superblock before its syncs changes only in-memory state and becomes wrong only if a later sync also fails. `checkpoint::run` therefore returns the `Durable` token required by adoption. A one-fault sabotage cannot enforce a two-fault ordering property that leaves no byte-level witness.

## Files

- `.agents/docs/JOURNAL.md` is the append-only record of experiments, reviews, and interpretations.
- `.agents/docs/TODO.md` tracks unresolved follow-up work exposed by investigations.
- `.agents/docs/TESTING.md` defines the end-to-end harness and measurement-fixture rules.
- `yesno-core/benches/` contains Rust benchmark entry points.
- `e2e/scenarios/` contains operational and measurement fixtures.
- `scripts/` contains repository checks whose own sensitivity must be maintained.

## Test Coverage

- Allocation tests provide mechanism-specific regression coverage where timings would be noisy.
- Oracle, differential, and expression-equivalence suites validate that optimized paths still compute the same result.
- End-to-end scenarios exercise lifecycle and storage behavior that isolated benchmarks cannot reach.
- Checker sabotage and positive-control exercises validate that gates fail when their targeted invariant is broken.
- Forced-path agreement tests validate the apparatus before a threshold or optimization is measured through it.
- A characterization test should be inverted or replaced by a positive regression when the defect is fixed.
- Process-wide counter tests should live in a dedicated test target when neighbouring tests perform the same class of work.

## Pitfalls

- Do not treat “the command passed” as evidence that the checker can detect its target failure.
- Do not compare results whose corpus, setup path, denominator, or units differ.
- Do not compare two libraries until both operands are forced into the representation named by the benchmark.
- Do not keep a benchmark name after refactoring makes its two arms call the same implementation.
- Do not time interpreter overhead and label it as a storage-engine or kernel cost.
- Do not let instrumentation change the ownership or optimization behavior that motivated the experiment.
- Do not promote a one-off number into architecture without recording code-state and corpus provenance.
- Do not keep a stale benchmark as a baseline merely because it is already documented.
- Do not quote a kernel figure from a fixture that intersects the same operand pair on every iteration.
- Do not assert on a struct's fields alone when the defect can live in the rendering. Field-level assertions cannot see a unit error in a display path, which is how a waste column was once printed in bytes under a KiB heading.
- Do not report a speedup measured once, and do not report a margin without stating what it is a margin over.
- Do not infer a stable scenario duration from one run on a loaded machine.
- Do not repeat only the inner timing loop when the conclusion depends on a multi-point sweep.
- Do not carry a number across a per-block, per-operand, per-query, or per-container boundary without restating its frame.
- Do not trust a process count whose search pattern appears in the polling command itself.

## A check whose success is not evidence it checked anything

The single most common defect found across this project is not a wrong answer. It is a check that **cannot fail**, reporting success having examined nothing. Instances accumulated faster than any other category:

- A scan that printed `0 findings` having walked `0 files`, because its path filter wanted a leading slash that relative paths do not have.
- A gate step that had been silently dead at 25 invocation sites, and reported success.
- A test whose `#[test]` attribute had been displaced, so it never ran; the test count stayed constant because a new test replaced it.
- A sabotage anchor that matched twice, so the sabotage was never applied and the run was reported as "passed ( bad )".
- A `--calibrate` corpus that verified two call shapes while the real corpus contained four; 30 % of call sites were invisible and a false positive on real code came out of it.
- An equivalence test over data whose structure made two different answers coincide — every terminal chunk held a single ordinal, so `min == max` inside it and returning the wrong end of the right chunk was undetectable.

- A SIMD suite in which **six kernels could be completely broken while the suite stayed green**, because every test reached them through a feature-gated dispatcher: on a host reporting the feature absent, every assertion degraded to `scalar == scalar`.

### The sharpest single instance

`ops::array` carried `assert_eq!( reached, 289 * 3, "the vector arm must actually be reached" )`. It *reads* as though it closes the question and does not: it counts operand pairs clearing the **length threshold**, and would be satisfied in full while every comparison ran scalar against scalar.

**A test asserting that a fast path was *selected* is not evidence that the fast path was *executed*.** Only sabotaging the kernel distinguishes them.

The evidence that settled it was a **two-condition** sabotage table: each kernel broken in turn, and the suite run twice — once on the real host, once with every dispatch gate rewritten to `false` to simulate a host without the feature. The second column is the whole point, and the fix is a direct-call wrapper added *alongside* the dispatcher assertions rather than replacing them. Host ground truth was established first: the machine really did report the feature present, so the degradation was latent there, not active. The defect was that nothing enforced it.

### What actually defends against it

**Print what the instrument looked at, beside what it found.** A verdict without a denominator is unreadable: `0 hits` and `0 hits over 2 085 definitions and 25 309 call sites` are different claims, and only one of them is falsifiable.

**Sabotage each branch the assertion claims to cover, not the function as a whole.** Breaking a whole function usually fails something; breaking one term is what finds the hole. A test that survives the deletion of the term it exists to protect is not testing it.

**Pair a detector with a positive control.** A poller that sees zero failures across a role change is green whether or not it can detect a failure at all. A second test that performs the real fault and *requires* the detector to fire is what makes the first one's silence mean something.

**Beware data whose structure makes distinct answers coincide** — a single-element container makes `min` and `max` agree, a single-chunk key makes flat and linear agree, a one-shard database makes sharded and unsharded agree.

**A guard whose removal changes nothing is not protecting anything yet.** An unwired-symbol sweep carried a documented rule that call sites must match a turbofish form as well as a parenthesis, justified by a named hot-path function that had once been reported dead. Removing the turbofish alternative changed the result by zero symbols -- because the cited function is declared with restricted visibility and the sweep's declaration pattern only captured fully public ones. The guard was real, the justification cited a symbol the sweep never scanned, and only running the sweep *without* the guard revealed the difference. A control that cannot fail is the same defect as a test that cannot fail, one level up in the toolchain.

**Say which population a count came from, or the count is not comparable.** The same sweep over fully public functions reports 15 with no caller; over every function it reports 1 214, most of them trait methods reached through dispatch and unmatchable by name. Those answer different questions -- what public API is unused, a versioning question, versus what code is dead, a deletion question -- and a later run quoting one number against the other concludes something false. Record the population beside the figure.

**Make the control cheap enough that demanding it is reasonable.** The same sweep took about ninety seconds per run while it scanned every file once per symbol name, and a quarter of a second after a single pass counting all names at once. Controls get skipped when they are slow; the restructuring is what made running it three times in a row -- planted symbol, guard removed, population widened -- an obvious thing to do rather than a cost to weigh.

**A miscalibrated diagnostic is worse than none.** A counter meant to measure a blind spot once reported 40 291 unattributable call sites by counting every `identifier( .. )` in the corpus — macros, closures, `Some` — and would have justified abandoning a working approach. Measure the instrument before believing the measurement.

### And for a bug that will not reproduce

Static analysis produces a coherent mechanism every time it is asked, and coherence is not evidence. Across two hard bugs this project spent four and then six rounds of reading, each yielding a self-consistent wrong answer; both were settled instead by one measurement against a live system. When a mechanism explains the evidence but has not been *observed*, it is a hypothesis — close the gap by printing the state the hypothesis is about, at the moment it matters, and waiting for the failure. Repeated whole-suite runs are a cheap detector for concurrency faults no single scenario reproduces, and the yield is not limited to the bug being hunted.

Report a rate only once the denominator exists. An intermittent failure called "two in roughly three runs" measured 2 in 9.

And once that rate is known, use it to judge the evidence that a fix worked. Four clean suite runs after fixing a failure that reproduced about 22 % of the time would happen roughly 37 % of the time even if nothing had been changed, so those runs are supportive and not conclusive. The load-bearing evidence for that fix was the measured divergence the instrumentation printed at the moment of failure, not the green runs afterwards. A handful of passes against a rare intermittent proves far less than it feels like, and the arithmetic to say how much is one subtraction away from the rate already measured.

## The unwired-symbol sweep, and what it structurally cannot see

The construction is recorded with the backlog item that owns it. What belongs here are the four limits, because each of them produced a wrong conclusion that looked right.

**A verdict list is a cache of an analysis, and nothing invalidates it when the code moves.** Such a list was kept so a later sweep could start from a verdict rather than re-derive one; within two weeks nine of fifteen verdicts had expired, all in the direction where a symbol had *acquired* callers, so the list understated how wired the crate was and a sweep trusting it would have re-flagged live code. It ages in the safe-looking direction too: a stale "no caller" reads as a to-do rather than as an error. And the premise died outright once the sweep was rewritten as a single pass and went from about ninety seconds to a quarter of a second — caching the result of a computation that cheap is strictly worse than repeating it.

**Classify callers by syntax, not by file path.** A unit-test module lives inside the production file, so an assertion there counts as a production caller unless the file is first split at its test attributes. That one omission understated the test-only set more than fourfold and did so silently, because every hidden symbol looked wired.

**An identifier sweep produces false negatives on common names, and a whole type can hide behind them.** One follower type was entirely unwired while every one of its methods looked used, because words like apply, visible and cursor occur elsewhere in the same crate. The sweep cannot see a type; it sees names.

**The obvious type-keyed version of the sweep is vacuous.** A type's own implementation blocks count as references to itself, so nothing is ever flagged: a first run reported seventy-four types and zero unwired, which was checked against the known-unwired follower and found blind.

The rule that follows from all four: never trust either kind of sweep without first running it against a subject whose answer is already known, and prefer re-running it to caching what it said.

The same caution applies to auditing a backlog, where the available mechanical check is weaker than it looks. Extracting every source-path citation from the open items and testing that each file exists reported zero dangling paths across twenty-six entries — while three of those entries were stale, every one of them citing a path that does exist. The check finds deleted files; it cannot find obsolete *reasoning*, which is the kind that costs real work. The productive form is manual and cheap: for each entry, name the one sentence that says why it is blocked, and go read the code that sentence is about.

One more thing about acting on a hit. An unwired function can be carrying a check that production genuinely lacks, so deleting it deletes the check. One such function validated a payload offset and could not simply be wired in, because its signature took a whole page by reference and using it on the read path would have copied four kilobytes per chunk and given up zero-copy. The resolution was to delete the function and re-express its check where the read path already had the values it needed, which cost nothing. Ask what a dead function was protecting before removing it, and carry the protection forward rather than the function.

## The always-identity-argument sweep

**The signature.** A parameter whose every call site passes the same literal. The function is called, the parameter is read, the constant has a documented default -- and the feature does not exist. No caller sweep can see it, because the function *is* called. Four instances were found by hand in the WAL on 2026-08-28, one of which made a documented 1 GiB checkpoint trigger unreachable: `should_checkpoint( dirty, 0, elapsed )`.

**The construction** ( rebuilt 2026-09-09, ~90 lines of Python, kept under `.agents-workspace/tmp/` because research does not ship ):

1. Regex every `fn name( .. )` under `yesno-*/src`, capturing parameter names. Detect a leading `self` and drop it, because a method's call site supplies one fewer argument and every position after it shifts.
2. Regex every call site across all `yesno-*` crates, taking the balanced parenthesis span and splitting on top-level commas ( tracking string, paren, bracket and angle depth ).
3. Skip names with more than one definition ( positions cannot be mapped ) and any name whose call sites disagree on arity.
4. Report a parameter whose value is identical *and* literal at every site: `0`, `None`, `false`, `true`, `""`, `&[]`, `Default::default()`.

**Result on a clean tree**: 0 hits across 1 625 definitions and 25 309 call sites.

### The calibration is not optional, and mine was too weak twice

**A scan that reports nothing is indistinguishable from a scan that saw nothing.** Two blindness failures, in one sitting:

* **The first run printed `0 parameter(s)` and had scanned `0 definitions`.** The path filter tested `/yesno-[a-z0-9-]+/`, and relative paths have no leading slash. Only printing the definition and call-site counts alongside the verdict caught it. **Always print what the instrument looked at, not just what it found.**
* **The canary corpus was insufficient, and it produced a false positive on real code.** The call-site regex used a lookbehind of `(?<![\w:])`, which excludes anything preceded by `:` -- so every **path-qualified** call, `record::encode_set_range( .. )`, was invisible. `encode_set_range( .., remove )` was reported constant `false` across "all 3" call sites when it has **8**, two of which pass `true`. The canary called everything bare and could not see the omission. Fixing the lookbehind raised coverage from 19 515 to 25 309 call sites -- **30 % of all call sites had been invisible**.

**The canary must contain every call *shape* the corpus does**, not merely a positive and a negative case. The four that matter here: a genuinely-always-identity argument ( must flag ), one that merely defaults ( must not ), a **method** call whose positions shift past `self`, and a **path-qualified** call. The instrument now carries them and self-checks with `--calibrate`, printing `CALIBRATION PASS` only when the verdict set matches exactly.

### The finding it produced

`RetentionFloor::holders( shard )` was `0` at all six call sites -- every one a test. Neutering the filter to `|(_, _s)| true`, so it counts the followers of *every* shard, broke **no test in the workspace** ( 33 passed ). The shard discrimination the method exists for was never exercised. Closed by `holders_counts_one_shard_and_not_the_others`, which registers followers on shards 0, 1 and 2 and asserts each count plus the total; it fails under that sabotage.

Its doc said "For metrics and for tests" and nothing under `yesno-server` renders it, so the doc was corrected rather than left to imply a metric that does not exist.

**The general rule this yields**: *a parameter that only ever takes one value is a branch that is never taken*. It is a test-coverage signature as much as a dead-feature one, and it is mechanically checkable.

