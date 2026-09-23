# The End-to-End Harness

`yesno-e2e` runs Python scenarios against a real `Db`, in-process, through
[monty](https://github.com/pydantic/monty). This document is about that harness:
what it is for, what may and may not go in it, and the traps it has already
sprung.

For *which* test layer a given change must land coverage in, see
`QUALITY_GATE.md` §3. For the layer catalogue as a whole, see
`ARCHITECTURE.md` § Testing Architecture. This file is the depth on one layer.

---

## 1. Why a scripting layer at all

yesno has ten test layers and every other one is Rust compiled against the
crate. That is the right shape for containers, kernels and the store, and the
wrong shape for the last question: whether the *operational sequences* hold up.
Open, ingest, checkpoint, close, reopen, query — and do the answers still match
what went in.

Those scenarios are dominated by their setup and their oracle, not by anything
the type system checks, and expressing them in Rust means a recompile per
variation. Here a scenario is a `.py` file under `e2e/scenarios/`. Adding
one is adding a file.

**The oracle is the real payoff.** Scenarios get monty's Python subset, so
Python's own `set` and `sorted` state the expectation:

```python
assert snap_load(s, 1) == sorted(pa & (pb | pc))
```

rather than reimplementing set algebra inside the assertion. That distinction —
*oracle*, not *second implementation* — is the whole discipline of this layer,
and §4 is about what happens when it is lost.

---

## 2. The sandbox

monty is not embedded CPython. It is a bytecode VM with **no ambient
authority**: filesystem, environment and network are absent unless the host
supplies them, and this harness supplies none. A scenario reaching for `open()`
is stopped with `E2eError::Sandbox` rather than writing somewhere unexpected.

Everything a scenario can touch arrives through a verb, so the surface under
test is exactly the surface the harness chose to expose.

Each scenario gets a fresh temporary root, removed on drop. Databases are plain
directory names inside it; `db_open("../escape")` is refused.

---

## 3. Two guards against a scenario that cannot fail

The runner refuses two shapes outright:

* a scenario whose source contains no `assert` statement, and
* a scenario that completes without calling a single verb.

**Neither guard can tell a weak assertion from a strong one.** Only
sabotaging the code under test can do that, which is how each scenario in
`scenarios/` was checked — see §8. What they catch is the empty file and the
scenario that quietly stopped reaching the database.

A third guard, `every_verb_has_a_caller_in_some_scenario`, is about the harness
rather than the scenarios; see §7.

---

## 4. What does **not** belong in `scenarios/`

> **The admission test.** Does a change to `yesno-core` fail this scenario *in
> the way its assertion is phrased*?

If the answer is no, it is not a scenario, however much it exercises the API.

On 2026-08-26 two files were migrated in and moved straight back out.
`and_shape.py` wrote three k-way intersection algorithms in Python;
`aligned_eval.py` wrote an aligned-grid evaluator. **Neither subject exists in
`src/`** — there is no `IntersectAll` operator and no aligned evaluator — so
`assert elim["pulled"] == 2 * CHUNKS` is a claim about the scenario file, and
`aligned_cardinality(...) == q_cardinality(e)` reads "my Python reimplementation
of set algebra agrees with yesno", which tests the reimplementation.

**Coupled is not the same as being about it.** Both files failed a sabotage
check — breaking `ops::and` did fail `and_shape.py`. The assertion that failed
was still phrased as a property of the Python walk; the coupling was incidental,
and an unrelated change to that walk would have "fixed" it.

**The structural tell**, which is cheap to compute and was visible the whole
time:

| | lines per verb call | `def`s |
|---|---|---|
| every scenario that belongs here | 2 – 4 | 0 – 5 |
| `and_shape.py` | 10 | 17 |
| `aligned_eval.py` | 11 | 31 |

A scenario is mostly calls and assertions. A file that is mostly function
definitions is an implementation, and the question is *of what*.

Those two live in `.agents-workspace/tmp/prototypes/` now. They still run:

```
cargo run --release -p yesno-e2e -- --show-output \
    .agents-workspace/tmp/prototypes/and_shape.py
```

---

## 5. The verb surface

The families are listed below; **the count is deliberately not written here**.
`yesno-e2e --list | wc -w` gives it, `world::all_names` — `NAMES` plus every
module's `OWNS` — is the source of truth, and
`every_advertised_verb_is_dispatched` keeps the lists and the dispatch honest.

**The number used to be written here and went stale three times** — once for
the whole `mx_*` family, and twice more inside a single day while `bn_*`,
`flight_*` and `srv_*` were landing. A figure that nothing checks, in a document
that already tells the reader not to trust it, is pure liability: it was wrong
more often than it was right. Do not restore it.
`every_verb_has_a_caller_in_some_scenario` guarantees the *verbs* are exercised;
nothing has ever guaranteed a hand-written count.

| family | module | what it reaches |
|---|---|---|
| `db_*` `snap_*` `batch*` | `world.rs` | the database: lifecycle, writes, batches, snapshots, fsck, space diagnostics, and a WAL-generation layout diagnostic scoped to its own temporary directory |
| `sb_*` `set_*` `ct_*` `ops_*` | `eager.rs` | `OrdSet` and `Container` directly, with no database in the picture |
| `q_*` `st_*` | `lazy.rs` | `Expr`, the planner, and the raw `ChunkStream` cursor |
| `mx_*` | `matrix.rs` | `BitMatrix` values, and the `OrdSet` boundary they are read across |
| `bn_*` | `bignum.rs` | `BigUint` values, and the `OrdSet` boundary they are read across. Every name a caller reaches for first — `pow`, `divmod`, `int`, `abs`, `min`, `max`, `len` — is a Python builtin, and `pow( a, e, m )` is *precisely the oracle these verbs are compared against*, so a dropped prefix would leave the scenario passing while testing nothing |
| `repl_*` | `repl.rs` | a real `LeaderService` on loopback and the shipped `FollowerClient` |
| `flight_*` | `flight.rs` | the shipped `YesnoFlightService` over a loopback socket, and a real `FlightServiceClient` |
| `fx_*` | `fixture.rs` | declared immutable resources, private paths, managed subprocesses, readiness probes, and interactive sessions for external fixtures |
| `op_*` | `operator.rs` | the checked-out operator and server in a disposable kind cluster |
| `fs_*` | `filesystems.rs` | deployed `yesnod` + `yesno-archive` over the local control socket, checkpoint-triggered direct ZFS/Btrfs/LVM snapshots, stateful Winterbaume S3, hot base backup, crash reconciliation, S3 restore, and restored-daemon reads in a disposable QEMU/KVM guest |
| `search_*` | `search.rs` | the Java application adapters and version-locked OpenSearch/Elasticsearch plugins, including pinned downloads and disposable engines |
| `srv_*` | `server.rs` | the shipped `yesnod` daemon, started in-process, plus the `yesnoctl` backup/restore and `yesno-archive` library entry points: lifecycle, metrics, auth and explicit control capability rules, control-plane checkpoint, replication/follower roles, hot-backup restore, object publication, and retention-gap rebootstrap |
| `yn_*` `clock_ns` | `world.rs` | the harness itself: constants, corpus knobs, a clock |

### Waiting for an asynchronous subject

**A poll loop written in a scenario is not a wait.** `failover.py`'s first
draft spun 412 times in 81 ms against a standby whose poll interval is a second,
so it gave up long before the subject had a chance to act. `srv_follower_wait`
does the waiting in the host with a real deadline, and **raises** on timeout
rather than returning — a scenario that carried on against a standby which never
caught up would fail somewhere later and blame the wrong thing.

It also gives up early when the standby has **halted**, because a halted standby
will never reach the target and waiting out the timeout would report the wrong
failure.

This is the same rule §1 states for timing loops, arriving from the other side:
what belongs in the host is not only measurement but any *wait*.

### Building a configuration a scenario can vary

`srv_config` / `srv_principal` / `srv_anonymous` / `srv_replication` /
`srv_archive_access` / `srv_archive_retention` / `srv_follows` / `srv_launch`
is a builder rather than a pile of keyword
arguments, and the reason is mechanical: principals are a **list**, and the
harness's argument conversion carries whole numbers, strings and lists of
numbers — not records. It mirrors `sb_new` / `sb_stride` / `sb_build`.

`srv_principal` takes the **token**, not its digest, and hashes it with the
shipped `auth::token_digest` — the same function the documented
`printf … | sha256sum` recipe has to agree with. A scenario pinning a digest
literal would be pinning a number this build might not compute.

### A refusal is a `RuntimeError`, a scenario mistake is a `ValueError`

`ValueError` means the *scenario* passed something wrong; `RuntimeError` means
yesno refused the operation. A server declining a call is squarely the second,
so `flight_*` verbs raise `RuntimeError` with the gRPC **code name** in front:

```python
except RuntimeError as e:
    assert "PermissionDenied" in str(e)
```

The name, not tonic's `Display`, which prints the code's *description* — "The
caller does not have permission to execute the specified operation". A scenario
asserting on prose breaks when tonic rewords a sentence, and the code is what a
client actually acts on.

### Flight and the daemon share one endpoint kind

`srv_flight` hands back the **same** handle kind `flight_serve` does, so every
`flight_*` verb runs unchanged against either a bare service or a real daemon.
That is deliberate: the same assertions apply to both, and a difference
between them is a finding rather than two test suites to keep in step.

The two differ in exactly one place, and it is the interesting one.
`flight_serve` is served from a database the *scenario* opened, so the harness
counts the service against that handle and `db_close` refuses while one is live —
a `Db` clone holds the directory's exclusive lock, which is the `live_snaps`
hazard arriving through a different door. `srv_start` opens its own database, so
a scenario must not `db_open` the same directory while it runs; it gets the same
`AlreadyOpen` an operator would.

### Server utilities stay real while the sequence stays declarative

`srv_basebackup` and `srv_archive_*` call the same public library entry points as
the thin binaries in `yesno-server-utils`. The host owns sockets, task lifetime,
filesystem traversal, and Protobuf decoding. Scenarios only express operational
order and compare durable results: a copied directory is reopened through the
ordinary `db_*` surface, and an archive wait validates its state manifest, base
objects, and generational WAL ranges before returning metrics. The basebackup
host uses only the control-plane lease RPCs; the default server provider stages
a portable immutable copy, while the scenario includes post-checkpoint WAL data
so reopening proves recovery rather than file presence. This avoids both
subprocess-only Rust tests and a second archive implementation in Python.
The archive scenario also waits until the durable event cursor proves its
subscriber is live while no base exists, then emits a checkpoint and requires
the base generation to appear. That ordering catches an eager startup capture
which would make the event subscription ornamental.

### Replication, and the async blocker that was not one

The `repl_*` family arrived on 2026-08-28, and the interesting part is what it
did **not** cost. `TODO.md` recorded that covering the M7 crates needed monty's
async host-call path — `FunctionCall::resume_pending` -> `RunProgress::ResolveFutures`
— "rather than the synchronous `resume` it uses today".

**That is only true of a scenario that wants to await two things at once.** A
host call is synchronous *from the VM's side*: monty calls out, the host does
whatever it likes, the host returns a value. Nothing stops the host from owning a
tokio runtime and blocking in it, and a scripted operational sequence — write,
serve, bootstrap, catch up, compare — wants exactly that: each step finished
before the next line runs. `ResolveFutures` is for suspending the *interpreter*
on a future, which no scenario here needs.

So `World` owns one multi-threaded runtime, built on the first `repl_*` call and
never otherwise, and each verb is a `Handle::block_on`. A scenario that does not
replicate does not start a runtime, which a unit test asserts.

Two rules the family keeps:

* **No verb hands a scenario WAL bytes to move itself.** The verbs drive the
  shipped leader and the shipped follower and nothing else. A scenario shipping
  frames in Python would be a second follower written in the scenario language,
  which is the §4 failure exactly.
* **The comparison stays in Python**, and is the payoff. A replica is a plain
  directory under the scenario root, so once it has caught up `db_open( name )`
  opens it as an ordinary database and every `snap_*` verb applies. Leader
  against follower is `snap_load( a, k ) == snap_load( b, k )`, and against the
  expected contents it is `sorted( ... )` of an expression the scenario wrote.

`e2e/scenarios/replication.py` costs about 3 s of the suite's runtime, nearly all
of it copying two shard images over loopback gRPC in a debug build. That is why
`replica_lag.py` — which is about the watermark rather than the bootstrap — has
no checkpoint and therefore ships no image, and runs in 0.4 s.

### Building operands without a database

`sb_stride( sb, base, step, count )` appends an **arithmetic progression** in one
host call, whatever `count` is. That is not a convenience; it is the shape these
operands actually have — `(p << 16) | l` over a prefix range is a stride of
65 536, a contiguous range is a stride of 1 — and it is the difference between
possible and impossible. A Python list of 8.6 million ordinals inside a bytecode
VM costs more than the rest of the scenario together. 1.8 M ordinals build in
13 ms.

Compose progressions and explicit values into one builder, then `sb_build`,
which sorts and dedups once. The builder is **consumed**: a second `sb_build` on
the same handle is an error, not a second identical set.

There is deliberately **no `set_range` verb**. yesno already spells ranges two
ways — `db_insert_range` is inclusive `[lo, hi]`, `q_range` is half-open
`[lo, hi)` — and a third spelling would be a trap. `sb_stride( sb, lo, 1, hi - lo )`
says which one it means. The harness mirrors both conventions rather than
smoothing them over, so `ranges.py` can pin each; a harness that normalized them
would be a worse oracle than the library it tests.

### `None` means empty

`ops_and` and friends return a container handle **or Python `None`**, mirroring
`Option<Container>`. `None` is "the result is empty", and it is the condition
every early-out in the engine branches on — so a scenario branches on `is None`
exactly where the Rust branches on the `Option`. It is never a zero-length
container.

### `st_open` does not plan

`Expr::open` plans then lowers; `Expr::open_planned` only lowers. `st_open` is
the second, deliberately: `st_open( q_plan( e ) )` is the planned column and
`st_open( e )` is the unplanned control. A verb that quietly planned would make
the two identical and any planner measurement vacuous.

---

## 6. Handles

Every object a scenario holds is an integer handle: databases, snapshots,
batches, queries, streams, sets, containers, set builders.

**Handles are typed and minted from one counter.** A handle is an index into
`World::handles`, which records its kind and its slot in that kind's table.
Passing a set where a query belongs raises
`q_cardinality(): handle 3 is a set, not a query`.

Do not rely on a handle being in range for the table you meant. Every kind's
first handle would be `0` if the counter were per-table, so a transposed
argument would resolve into the wrong table and answer about a different object
— which is why the counter is shared and the tag is checked.

**Five kinds can be invalidated** — database, snapshot, batch, stream, set
builder — and that is why a handle table beats a Python object: a closed
database must raise on next use rather than resurrect a stale `Db`. Sets,
containers and queries are frozen values with nothing to release.

`db_close` refuses while a snapshot from that database is alive, and says so.
A `Snapshot` transitively holds the store — and the directory's exclusive lock —
open, so closing underneath one would fail several statements later inside
`Db::open` with a lock error pointing nowhere near the mistake.

---

## 7. Three naming rules

Each exists because breaking it produced a scenario that could not fail.

1. **No verb may collide with a Python builtin.** `open`, `min`, `max` and
   `len` are all natural names here and all wrong: monty resolves builtins
   **without ever asking the host**, so `min(snap, 7)` would silently return the
   smaller of two integers instead of the key's minimum ordinal, and
   `open("db")` would become a sandboxed filesystem call. Hence the `db_` /
   `snap_` / `q_` / `set_` / `ct_` / `st_` prefixes.
   `no_verb_collides_with_a_python_builtin` enforces it.
2. **An unknown verb is a `NameError`, never a `None`.** A typo'd verb that
   returned nothing would let a scenario pass having done nothing.
3. **An unknown keyword argument is an error.** `db_open("x", shard=4)` must not
   silently open the default 8.

And one rule about the surface as a whole:

4. **Every verb must have a caller in some scenario.**
   `every_verb_has_a_caller_in_some_scenario` enforces it. A verb whose last
   caller goes away otherwise keeps compiling, keeps passing its unit tests, and
   stops being exercised by the suite with nothing failing.

The guard checks a verb is *mentioned*, not that it is meaningfully
   exercised. A scenario calling `set_rank` and ignoring the answer passes it.

---

## 8. Writing a scenario

1. **State the expectation in Python's own types.** `sorted(pa & pb)`, not a
   loop that recomputes the intersection.
2. **Always reopen before reading durable state.** `checkpoint()` does not clear
   the memtable, so a read on the same `Db` instance is answered from memory and
   never reaches the store. That blind spot hid three separate bugs.
3. **Assert the finding, not the setup.** Where a fixture printed two columns
   and left the reader to compare them, compare them. `ckpt_under_reader.py`
   asserts that a held reader leaves the deferred list strictly larger and that
   releasing it drains it — the example only printed both.
4. **Sabotage the code under test before landing it — and know what that does
   not prove.** Revert what the scenario pins; confirm it goes red, and confirm
   unrelated scenarios stay green. A scenario that has never been observed
   failing is worth nothing.

   Nine sabotages check this suite, each caught by the intended scenario and by
   no other:

   | sabotage | caught by |
   |---|---|
   | a planner that rewrites nothing | `rule_economics.py` |
   | `insert_range` made exclusive at the top | `range_ingest.py` |
   | `safe_version` ignoring live readers | `ckpt_under_reader.py` |
   | `delete_key` made a no-op | `aged_state.py` |
   | WAL replay skipped at open | `wal_size.py` |
   | `union_all` dropping its last operand | `nary_or.py` |
   | `partition_point_in` off by one | `set_api.py` |
   | `next_cardinality` off by one | `streams.py` |
   | `SetStream::seek` made to rewind | `streams.py` |
   | `FetchBaseSnapshot` reporting the log's end as the replay offset | `replication.py` |
   | `WriteBatch::commit` not writing a `CommitIntent` | `replica_lag.py` |
   | a stale follower cursor answered with a heartbeat | `steady_state.rs` ( Rust ) |
   | `bootstrap_shard` leaving the old cursor in the tracker | `steady_state.rs` ( Rust ) |
   | the checkpoint not persisting the log's base lsn | `durability.rs` + `steady_state.rs` ( Rust ) |
   | generation rollover omitted, restarting, or breaking the LSN sequence | `wal_generations.py` + `writer.rs` + `steady_state.rs` ( Rust ) |
   | `enforce_policy` passing `0` for the WAL-size trigger | `durability.rs` ( Rust ) |

The generation-rollover row is the instructive one, and it is about *where*
   a sabotage is caught rather than whether. `durability.rs` **survives** it: it
   reopens each round, and a reopen repairs the base from the superblock. The
   bug it misses is LSNs reused *within one process*, between a checkpoint and
   the next restart — which is exactly the live-follower case, and only the test
   that never reopens the leader can see it. Two tests, two axes, neither
   sufficient. A sabotage caught by *some* test is not the standard; caught by
   the test whose subject it is, is.

The `CommitIntent` one is worth its own note: when it was found, removing
   the record entirely left **the whole of `yesno-core`'s suite green** — 552
   unit tests and every integration test, the crash matrix included — and
   `catch_up.rs` green too, since it runs at one shard. A multi-shard commit
   resolving on the first participant alone was invisible to every layer until a
   follower was asked to catch shards up one at a time.

That is a finding about *where a gap gets found* versus *where it belongs*.
   The defect was in recovery, not in replication: `recover` derives `global_cv`
   from "every shard in `CommitIntent.shards` has a CRC-valid `ShardCommit{cv}`",
   and no test exercised the branch where one does not. It is now pinned at its
   own layer by `durability.rs::a_multi_shard_commit_missing_one_participant_is_discarded_on_both`,
   which cuts one shard's log tail and asserts that the *other* shard's complete,
   CRC-valid, durable half is discarded too. Leaving it caught only by the
   replication layer would have made a `yesno-core` change fail in a satellite
   crate, which is the wrong place to learn it.

**Passing a sabotage check does not make something a test of yesno.** Both
   files moved out in §4 passed one — breaking `ops::and` did fail
   `and_shape.py`. The assertion that failed was still phrased as a property of
   the Python walk; the coupling was incidental, and an unrelated change to that
   walk would have "fixed" it. Sabotage is the standard defence and it is
   **necessary, not sufficient**: it shows the instrument *moves* when the
   subject breaks, not that the instrument is *measuring the subject*. Apply §4
   as well.
**And a sabotage that *passes* has two opposite diagnoses, which nothing about
   the pass distinguishes.** Everything above assumes the sabotage goes red and
   asks which test caught it. When none does, the instinct is "the test is
   weak", and that is right only half the time. Both cases turned up on
   2026-09-13, hours apart:

   * **Weak injection, sound test.** `ChunkSource::occupancy` was sabotaged to
     report every *other* prefix, and nothing failed. The test was fine: a
     `PrefixOccupancy` is 256 buckets, so dropping half the prefixes almost
     never empties one. Sabotages that did empty buckets -- first half only, one
     prefix only -- both failed correctly, and the second named exactly the
     dropped range.
   * **Weak test, sound injection.** An allocation test asserted `>= 800` to
     prove segmentation still engaged. Suppressing segmentation entirely left it
     green, because 800 sits below **both** the segmented figure ( 1 038 ) and
     the suppressed one ( 910 ). The injection was total; the threshold guarded
     nothing.

   Same symptom, opposite defects. Two rules follow, and the second is the one
   that would have prevented the second case:

   * **An injected fault must survive whatever summarizes it.** Between a fault
     and the assertion there is often a lossy layer -- a bucketed sketch, a
     cardinality, a checksum, a count. Size the fault to be visible *after* that
     layer, or a pass says nothing about the test.
   * **A threshold must sit strictly between the two measured values, and both
     must be known before it is chosen.** `>= 800` was picked as a round number
     under the expected result rather than derived from the pair it had to
     separate. **If you cannot say what the assertion reads under sabotage, the
     test is not designed yet** -- and for a *lower* bound, which asserts a cost
     is still being paid because the failure mode is an optimization silently
     not happening, this is the only thing standing between the test and
     vacuity.
5. **Prefer the structural claim to the string.** `q_repr` tells you *that* the
   planner changed something; `q_kind` says *what* it produced.

`try` / `except ValueError` works, so negative contracts — a stale handle, a
transposed one, an out-of-range child index — are assertable too.

---

## 9. Measurement fixtures

`scenarios/` also holds the fixtures that were `yesno-core/examples/*.rs` until
2026-08-26. They moved because **no gate ever executed an example**:
`cargo clippy --all-targets` type-checks one, which catches a rename and nothing
else, so a fixture whose subject silently changed kept compiling and kept
reporting a number nobody re-derived. As scenarios they run in
`cargo test -p yesno-e2e`.

`examples/readme.rs` stayed behind, and must: its whole value is being *Rust
that compiles*, which no scenario can check.

Running them at the corpus their recorded numbers came from:

```
cargo run --release -p yesno-e2e -- --show-output \
    --arg keys=200 --arg per_key=2000 --arg rounds=60 \
    e2e/scenarios/aged_state.py
```

* `--arg name=value` sets a whole-number knob read by `yn_arg( name, default )`.
  This is what replaced `--big` / `--sparse`. Absent under `cargo test`, so the
  gate always runs a small corpus and the large one is a deliberate act.
* `--show-output` prints what a *passing* scenario printed. A failing one always
  shows its output. Without this a measurement fixture's table is invisible.

### Timing

**A repetition loop written in Python times monty.** Measured: ~1 µs per host
call and ~100 ns per VM instruction in release ( 6.7 µs and 650 ns in debug ),
against the tens of nanoseconds a planner measurement is about.

* Where the subject is an `Expr`, use **`q_time( expr, iters, terminal )`**,
  which runs the loop in the host. Terminals: `plan`, `cardinality`, `collect`,
  `drain`. It returns `{ns, iters, truncated}` and bounds itself by wall clock
  as well as by count, reporting the iterations it **actually ran** — the
  runner's time limit only bounds the VM, so a host call that loops is invisible
  to it.
* Where the subject is a hand-written walk, it cannot be timed — and per §4 that
  walk should not be in `scenarios/` at all.
* `clock_ns()` is fine for millisecond-scale columns ( ingest rates, checkpoint
  latency ) and useless below that.

If you extend `q_time`, keep its per-iteration work to the terminal itself.
Resolving the terminal name per iteration costs ~29 ns and reading the clock per
iteration ~25 ns — both larger than the quantities it is used to report, and
both show up as a flat offset on small rows while large rows look correct. The
terminal is resolved once and the deadline is checked once per batch, sized from
the warm-up.

---

## 10. Running it

```
cargo test -p yesno-e2e                  # the suite, as a gate
cargo run -p yesno-e2e --bin yesno-e2e                   # the same suite, by hand
cargo run -p yesno-e2e --bin yesno-e2e -- one.py two.py  # just these
cargo run -p yesno-e2e --bin yesno-e2e -- --list         # the verbs a scenario may call
cargo run -p yesno-e2e --bin yesno-e2e -- --show-output --arg n=100000 e2e/scenarios/wal_size.py
./scripts/gate-pg.sh                       # includes e2e/postgresql/postgres.py on PG 17 and 18
./scripts/gate-mysql.sh                    # includes e2e/mysql/mysql.py against pinned MySQL 8.4
./scripts/gate-search.sh                   # Java helper plus pinned OpenSearch and Elasticsearch
./scripts/gate-operator.sh                 # live yesno-operator lifecycle in a disposable kind cluster
./scripts/gate-filesystems.sh              # deployed archive/S3 lifecycle on real ZFS, Btrfs, and LVM in KVM
./scripts/gate-mount-propagation.sh        # agent/daemon mount propagation under Docker's own flags
AWS_REGION=ap-northeast-1 ./scripts/gate-aws.sh # Terraform-owned live EC2/EBS lifecycle: local, deferred/ECS, deferred/EKS
```

`cargo test` gives no way to run one scenario and see its output, which is
exactly what writing a new one needs — hence the binary. Both go through the
same `run_file`, so a scenario behaves identically either way.

PostgreSQL and MySQL are ABI-scoped exceptions to Cargo discovery, not to
the harness architecture. `e2e/postgresql/postgres.py` and
`e2e/mysql/mysql.py` are selected by Bazel because their extensions are
meaningful only against the exact server ABI Bazel built. Both targets invoke
the ordinary Monty runner, link the same featureless fixture host, and pass
immutable resources explicitly. The scenarios compose the shared `fx_*`
utilities for private writable copies, command execution, managed servers,
readiness, interactive input, transcript waits, exact fixture comparison, and
cleanup. Backend identity and lifecycle remain Python data; there is no
PostgreSQL or MySQL Rust world, Cargo feature, binary entrypoint, or shell
launcher.

PostgreSQL additionally starts the shipped Flight service in-process and uses
`fx_start_tty` for line-flushed, marker-synchronized `psql` sessions, preserving
byte-exact two-session isolation output without host `stdbuf`. MySQL initializes two private copies of the pinned install. Embedded mode
runs the C ABI backend and byte-exact `mysqltest` corpus; Flight mode loads the
native C++ client against the same in-process Flight primitive used by
PostgreSQL. Both modes assert the engine's schema, boundary, scan, mutation,
aliasing, transactional rollback, and plugin-lifecycle contracts. Both suites keep
every mutable file under the scenario's temporary world.

Operator integration is an opt-in scenario through that same runner, not a
second binary or fixed Rust lifecycle. `e2e/operator/operator.py` sequences the
`op_*` verbs and owns every assertion. The narrow verbs in `operator.rs` build
and load the checked-out images, own a disposable named kind cluster, isolated
kubeconfig, bounded waits, diagnostics, and cleanup, and return Kubernetes and
yesno observations to Python. `scripts/gate-operator.sh` selects the scenario
explicitly and makes Docker the only host prerequisite. It builds one driver
image containing the ordinary runner, operator, kind 0.33, kubectl 1.36.1, and
the Docker client. The driver mounts the host Docker socket, joins kind's Docker
network, and uses an internal kubeconfig; this is neither Docker-in-Docker nor a
privileged container. It covers
invalid-spec status without child resources, a pinned cert-manager install and
disposable CA issuer, per-instance Certificate issuance, exact Ready resource
shape, real ingest and checkpoint through a dedicated mTLS client Pod, fenced
automatic promotion over mTLS, data survival after promotion, generated
Certificate and private-key Secret deletion, and retained-PVC survival.

It also covers the EBS snapshot backend's discovery path, and covers it by
asserting a **refusal**. `op_enable_ebs_snapshots` patches the running cluster
to `spec.snapshot.backend: ebs` and waits for the `SnapshotBackendReady`
condition. kind has no EBS, so every step up to the volume is real -- the CRD
accepts the spec, the controller's ClusterRole permits the cluster-scoped
PersistentVolume read, and discovery walks each instance's claim to its bound
volume -- and only the answer differs from EKS: kind's `standard` class
provisions a local-path volume, so the correct outcome is `UnusableVolume`, the
generated ConfigMap keeps the portable provider, and the cluster stays `Ready`.
The last of those is the assertion worth keeping: a controller that raised
the unreadable volume as a reconcile error would take failover down because a
*backup* setting could not be resolved. This does not cover a real EBS
volume, a generated `[server.snapshot]` section, or a snapshot; the real-AWS
gate's operator arm covers the first two ( below ) and nothing yet covers the
third. Set
`YESNO_E2E_KEEP_KIND=1` to retain the cluster and unique images for diagnosis,
or `YESNO_E2E_CERT_MANAGER_MANIFEST` to use a local copy of the pinned release
manifest.

### The operator arm of the AWS gate

`op_prepare` takes a backend, and `"eks"` is the second one. It owns nothing:
the real-AWS gate has already provisioned a cluster, pushed the images and
written an administrative kubeconfig, and this backend is handed all of it
through `YESNO_OPERATOR_*` -- so what it adds is the CRD, cert-manager, the
operator, and the same `YesnoCluster` lifecycle against storage that is
genuinely Amazon EBS. `e2e/aws/operator.py` owns the assertions.

**The two backends exist for one assertion each, and neither subsumes the
other.** kind covers cert-manager, promotion and the fence, and can only ever
reach `UnusableVolume`. The EKS arm covers what storage decides and repeats
none of the rest -- no failover, because it costs billable minutes to re-prove
something storage has no part in. One `cluster_manifest` builds both, so a
divergence cannot let the cheap arm pass on a manifest the expensive one never
runs.

Green on 2026-09-05 ( `ok operator.py`, 15 verb calls, 89 s ), after three
runs that each found a real defect first -- a follower that could not survive
an interrupted bootstrap, and a claim too small to hold a materialized shard
image. Neither was findable on kind, and both were in storage, which is the
one variable the two arms do not share.

What the EKS arm asserts that nothing else can: `SnapshotBackendReady` reaches
`Configured`; each of the two instances names a **different** `vol-`; and that
list, parsed out of the ConfigMap the daemon mounts, equals the CSI volume
handles of the PersistentVolumes the claims bound to. The cross-check is the
point -- one side is the controller's output and the other is Kubernetes, so a
volume id invented, defaulted or transposed makes them differ.

It also requires the rollout to have **settled**: every instance's running
Pod must carry the config identity its Deployment specifies. Learning the
volume rewrites the ConfigMap and the Pod template in the same reconcile that
patches the status, so `SnapshotBackendReady=True` is observable before the
Recreate that carries it into a daemon has started. Without that check the arm
could pass on a configuration nothing had read.

**One assertion there is one-sided and says so.** `reconcile_errors == 0`
reads the daemon logs for the message a Pod without working AWS credentials
produces. Startup reconciliation retries in the background and the Pod stays
Ready either way, so a non-zero count proves EC2 was not reached and zero does
not prove it was. Do not read it as proof that IRSA works; what would prove
that is a snapshot lease taken from inside the Pod.

Three things make the arm's cleanup its own concern. The controller
deliberately does not owner-reference a database claim, so nothing deletes the
EBS volumes when the `YesnoCluster` goes; the runner script therefore deletes
the namespace and then **waits for the volumes to actually disappear**, because
a `terraform destroy` that removes the cluster in between orphans them. And
`scripts/gate-aws-destroy.sh --orphans` now sweeps both namespaces for the same
reason.

Two static checks exist because this gate's failures are the most expensive to
discover. `every_opt_in_scenario_compiles` compiles every `.py` under the
opt-in directories -- including `gate.py` itself -- so a syntax error is a
`cargo test` failure rather than a container failing twenty billable minutes
into a run. `scripts/check-runner-scripts.py` does the same for the shell:
every remote step is a Python string run by `/bin/sh` on the instance, and
nothing between here and there parses it. Both are syntax only.

Native-filesystem integration is another opt-in use of the same all-in-one
image and the same Monty entrypoint.
`e2e/filesystems/{zfs,btrfs,lvm}.py` owns the assertions; `fs_*` verbs own a
bootable QEMU guest, a framed serial TTY, bounded
readiness waits, and calls to the shipped daemon and Protobuf clients. The
driver image carries the guest kernel, a read-only qcow2 backing image, QEMU,
all three storage toolchains, the utility binaries, and a pinned Winterbaume
server. Each scenario adds a private root overlay and raw data disk, runs QEMU
with KVM, drives its root shell through a private Unix serial socket, and
reserves TCP forwarding for the Flight and control-plane protocols.
Winterbaume runs beside QEMU with its filesystem VFS, so sparse 1 GiB database
images do not accumulate in the fixture's memory; the guest reaches it through
QEMU's host gateway. It requires no host mount, filesystem module, privileged
container, SSH handshake, DHCP lease, or external S3 account. The gate
deliberately has no emulation fallback: missing or unusable `/dev/kvm` is a
failed prerequisite rather than a slow test with different timing and failure
modes.

All three scenarios start the shipped sidecar as a systemd service against
`unix:///run/yesno/control.sock`, proving local-channel authorization and the
shared Unix client transport. They require a fresh archive to remain baseless
until an explicit checkpoint event, then require a direct ZFS/Btrfs/LVM snapshot,
S3 base publication, post-base WAL cursor advancement, graceful SIGTERM and
immediate writer-lease reacquisition, another WAL era, and an S3 restore whose
new daemon returns the exact expected sets. Reading the local Protobuf
`state.pb` only supplies bounded wait observations; the sidecar publishes the
remote objects and CAS-fenced state before that cache, and the final independent
restore is the consistency oracle.

The LVM scenario additionally starts `yesno-snapshot-agent` on that exact Unix
socket. Its systemd unit runs as root with only `CAP_SYS_ADMIN`; an LVM-specific
drop-in gives `yesnod` an empty capability bounding set. Successful capture,
materialization, release, and crash reconciliation therefore exercise the
all-zero `SCM_CREDENTIALS` admission path and prove the daemon itself cannot
perform the privileged commands. Unit tests separately pin that ordinary RPC
requests are denied and that checkpoint progress remains blocked until the
matching capture completion, but not during materialization.

The same scenarios retain the lower-level lease controls: provider identity,
explicit direct-path opt-in, a network-streamed hot backup reopened through
ordinary `Db`, release-time provider-object deletion, and startup
reconciliation of an object orphaned by SIGKILL. The network backup never
consumes the direct path: guest paths are valid only for the co-located sidecar.
This opt-in gate transfers each sparse shard's full logical image through S3, so
it is intentionally heavier than the ordinary in-process archive scenario.

The filesystem scenarios currently take about 75 s together on ARM64
KVM after the driver image is built. The filesystem gate gives each run a
1,200 s harness deadline so image-sized S3 transfers fail with diagnostics
rather than being confused with the ordinary scenario timeout.

`e2e/filesystems/lvm_propagation.py` is the fourth scenario in that gate and
the only one whose subject is a deployment topology rather than a provider. It
runs `yesnod` under `PrivateMounts=yes` — a slave mount namespace, which is
what Docker's `bind-propagation=rslave` and Kubernetes'
`mountPropagation: HostToContainer` produce — with the privileged agent in the
host namespace, and requires the lease to still be serviceable. Its
load-bearing assertion is `isolated`: a daemon sharing the host's namespace
sees every snapshot mount trivially, so without that check every other
assertion in the file would pass on a topology that proves nothing.
Sabotage-checked with `MountFlags=private` on the daemon, which fails the
scenario at `fs_snapshot_begin()` with a permission error rather than a mount
count — the daemon reaches the agent's `0700` pre-mount directory instead of
the mount.

Ordering is part of the test, not incidental. A mount namespace receives
only mounts made *after* it exists; one created later inherits a copy of the
table and would see the mount whatever its propagation says. The daemon is
therefore started before any lease exists, which is also the real sequence.

Real-AWS EBS integration is a separate opt-in gate selected by
`scripts/gate-aws.sh`, and since 2026-09-02 it is **four** scenarios rather
than one. Terraform under `e2e/aws/` still owns the network, ingress-free SSM
runner, run-scoped IAM role, encrypted source volume, temporary ECR repository,
ECS cluster, Fargate task definition, EKS cluster and node group, and the
shared EFS filesystem — declarative resources, whose graph is what makes
destroy exact. Everything imperative around them moved into `e2e/aws/gate.py`,
which runs on the host through the ordinary Monty runner: it applies the stack,
reads the outputs, pushes the runner image, waits for the Systems Manager
channel, provisions the runner, ships `e2e/aws/ebs.py`,
`e2e/aws/deferred_ecs.py` and `e2e/aws/deferred_eks.py` to it, and destroys.

**This gate provisions an EKS cluster by default.** A cluster and a node
group are roughly fifteen minutes to create and ten to destroy, on top of
everything else, which puts a whole run somewhere over an hour. `YESNO_AWS_EKS=0`
skips that arm and buys the time back.

**The switch defaults on, and only an explicit `0` turns it off** — a typo
leaves the gate testing more rather than silently less. Do not flip the
default or set it in CI: this is the only live coverage the deferred EKS path
has anywhere, and an arm that is off by default is an arm that rots. A run with
it set does not advance TODO.md's live deferred-materialization item, and
`gate.py` says so on stdout rather than letting a short green run read as a
full one.

The flag reaches Terraform through the run's `gate.tfvars`, not through a
`-var` flag, for the same reason the architecture does and then some: the
wrapper's signal backstop destroys with that file, and a `destroy` that
disagreed with the `apply` about whether a cluster exists would either fail on
a resource the state does not have or leave one standing that nothing else will
remove. `cloud_config()` returns the flag, `gate.py` branches on it, and every
EKS output is read **inside** that branch — three of them come from a resource
that may not exist, and `cloud_output()` refuses an empty value.

The `cloud_*` verbs behind it are deliberately **intrinsics** and know
nothing of that sequence: one Terraform subcommand, one output, one image push,
one wait, one remote command. Both shell scripts the gate used to carry are
gone, and so is the `.tftpl` that rendered a ninety-line runner through
cloud-init — the runner script is now text in `gate.py`, so changing what the
runner does is editing a `.py` file. The one thing the scenario may not
restate is the environment the runner image needs: `cloud_runner_env()` builds
it from the same list `aws_prepare()` reads back, and the scenario composes its
`docker run` flags by iterating that dict. The two halves run on different
machines, separated by a twenty-minute billable round trip, which is too
expensive a place to discover a missing variable. `cloud_deferred_env(kind)`
does the same job for each deferred arm, with one difference worth knowing: the
names it fills are `yesno-archive`'s **own** documented variables — plus
`KUBECONFIG`, which is Kubernetes' — so handing them to the shipped binary is
also the assertion that a deployment configured through the documented surface
can materialize a provisional lease. Each arm's table is unit-tested against
`aws::archive_env()`, and the two arms' containers are given only their own
arm's variables.

**Why it is split into a provision command and a run command.** A single opaque
SSM invocation reported one thing: non-zero. Resolving the source EBS device is
the step most likely to break on a new instance family, and it now fails as its
own named step with the runner's `lsblk` table attached. The scenario also
asserts the runner's summary line exactly ( `1 scenario(s): 1 passed, 0
failed` ), because a zero exit with nothing executed — a stale image, a
scenario file missing from it — is otherwise indistinguishable from a pass.

**The gate creates billable resources, so every `cloud_*` verb refuses
without `YESNO_AWS_GATE=1`.** `scripts/gate-aws.sh` sets it and nothing else
does. Without that guard, `every_advertised_verb_is_dispatched` — which calls
every advertised verb with no arguments — would provision AWS during
`cargo test`. Destroy is guaranteed twice: a `Drop` on the host side runs it
when a scenario fails before reaching `cloud_terraform("destroy")`, and the
wrapper keeps a `trap`-based backstop because a destructor does not run on a
signal. Both destroy through the same generated `gate.tfvars`, so the
architecture the Docker host selected cannot differ between them.

`e2e/aws/ebs.py` is the specification. The `aws_*` verbs start the shipped
daemon directly on the mounted source EBS filesystem, use ordinary Flight and
control RPCs, and observe AWS resources through the same SDK version as the
provider. The scenario asserts exact tagged resource counts before, during,
and after a file-bearing lease, reopens a streamed base backup, then kills the
daemon with a live lease and requires startup reconciliation to remove both the
snapshot and clone.

**This is the only gate anywhere that executes an EBS mount**, so it also
owns the privilege assertions for that path. The harness runs `yesnod` under an
unprivileged account and `yesno-snapshot-agent` as root beside it, and
`aws_privileges()` reads both processes' effective uid and `CapEff` out of
`/proc` — not out of what the harness believes it spawned. Until 2026-09-01 it
ran the daemon as root, which is why a provider step that needed
`CAP_SYS_ADMIN` in the daemon could not have been caught here. The scenario
also asserts that the lease is served from a real mount below the configured
snapshot directory and that the attachment name came from the agent's own
configured pool; the daemon never names a device, so a name from outside the
pool means the agent has started trusting its caller. The agent is deliberately
not restarted with the daemon, so the post-crash reconciliation runs through a
re-established agent connection and the scenario pins the agent's pid across
it.

`e2e/aws/deferred_ecs.py` is the other arm of the same backend, and the reason
the gate is worth two runs on one instance. There, `materialization =
"deferred"` means `yesnod` restores nothing: it returns a provisional snapshot
descriptor, and `yesno-archive` launches an ECS Fargate task that mounts the
restored volume, copies the bounded database file set onto EFS storage both
processes see, and exits. The archiver remains the only object-store publisher.

**The deferred arm's central claim is an absence, and the scenario states it
as one.** `aws_prepare_deferred()` renders a configuration with no
`unix_socket`, no `mount_dir`, no `instance_id`, no `availability_zone` and no
`device_names` — every field the daemon documents as needed only by local
materialization — starts no snapshot agent, and runs in a container with no
`--privileged`, no `/dev`, and no propagation flags. `aws_privileges()`
therefore reports `agent_pid is None`, and between the two arms the suite says
*the privilege is where the mount is*. Do not hand the deferred container a
mount it does not need to make a failure go away; the container's own shape is
part of the assertion.

The scenario runs the failure path first, deliberately: it points the archiver
at a source path the task will not have, so `yesno-snapshot-stage` exits
non-zero inside the same image, the same task definition and the same launch.
It then requires the archiver to exit naming the container's failure, the
server to have deleted the snapshot it owned, ECS to have deleted the volume it
restored, and the staging filesystem to be empty. Only then does it run the
successful half. Both halves end at the same three observations, which is what
makes the pair mean something: a cleanup that only works after success is not a
cleanup.

**The materializer's own volumes cannot be scoped to one run.** ECS tags them
from a lease token `yesno-archive` derives internally, so `aws_materializer_volumes()`
filters account-wide on `yesno:lease` and subtracts anything also carrying this
run's tag — which only provider-created resources have. Two deferred gates
running at once in one account would count each other's volumes. The gate is
opt-in and serial; narrowing it would need a tag the shipped code does not set.

**A worker outliving an interrupted run is worse than a leaked snapshot**:
`terraform destroy` cannot delete a cluster that still has an active task, so
the whole stack would stay standing and billing. The deferred run's shell trap
therefore stops running tasks in this run's cluster before it exits, and the
runner's role may stop tasks in that cluster and nothing else.

`e2e/aws/deferred_eks.py` is the second deferred materializer, and it is not
the same test with a different launcher. ECS restores the snapshot with a
managed volume and an infrastructure role; Kubernetes restores it with a
retained `VolumeSnapshotContent`, a `VolumeSnapshot` bound to it, an EBS-CSI
`PersistentVolumeClaim`, and a Job on an EC2 node — four objects, three
controllers, and a different failure at every step. The two archiver code paths
share only the lease. The scenario keeps the same shape as the ECS one: failure
half first, and both halves ending at the same three observations.

**How the cluster is bootstrapped is itself a decision worth not undoing.**
There is no kubectl on the runner and no Kubernetes provider in the stack.
kubectl would be a downloaded binary on a disposable host; the provider would
need a working cluster client at *plan* time to create objects whose CRDs the
same apply is still installing — the ordering problem that makes people split
the stack in two. Instead the runner's instance role is a cluster administrator
through an EKS access entry, `aws eks get-token` turns that into a bearer
token, and `gate.py` ships a script that POSTs JSON to the API server with
curl. Do not replace this with a `kubernetes` provider block without solving
that ordering; it is why the objects are created from the runner rather than
from the stack.

The archiver's own credential is a namespace-scoped ServiceAccount token from
the TokenRequest API, written to a file on the instance and bind-mounted into
the container read-only. It is never a Terraform output and never travels
through Systems Manager, where a command document keeps its parameters. Its
RBAC is exactly what `yesno-archive` uses — create, get and delete on
cluster-scoped `volumesnapshotcontents`, and the same three verbs on
`volumesnapshots`, `persistentvolumeclaims` and `jobs` in one namespace. A
cluster-admin token would have proved the materializer works and said nothing
about what it needs.

**The EKS arm's volume filter is a tag the EBS CSI driver writes, not one we
set** — `kubernetes.io/created-for/pvc/namespace`, which exists because the
driver defaults to `--extra-create-metadata`. Were it off, the filter would
find *nothing* rather than the wrong thing, and every "no volume was left
behind" assertion would pass vacuously. Both deferred scenarios therefore
require a **nonzero** count while the volume exists, not only zero afterwards:
`aws_materializer_volumes()` takes a range for that reason. Do not reduce it
back to an exact count.

Three parsers back those assertions — `/etc/passwd`, `/proc/<pid>/status`,
and `/proc/self/mountinfo` — and none of them can run outside the runner. They
are therefore split into pure functions with unit tests in `yesno-e2e`, because
a parser that silently returns "no capabilities" would turn the strongest
assertion in the suite into a tautology. `yesno-aws-cleanup` is an independent failure-path helper:
the runner invokes it from a shell trap, filtering on the unique
`yesno:e2e-run` and `yesno:e2e-object=lease` tags, before Terraform destroys
its own resources. Requiring both tags prevents the helper from matching the
Terraform-owned source volume, which intentionally carries only the run tag.

Both materialization modes' rendered `yesnod` configuration is parsed and validated by the
daemon's own loader in a `yesno-e2e` unit test. This is worth a temporary
directory: a section header in the wrong place still produces valid TOML, and
the mistake would surface on an EC2 instance twenty billable minutes later as a
missing region. It found a real one — until 2026-09-02 the gate's configuration
had no `[[auth.rule]]` at all, and the shared control endpoint refuses to start
without one, so the first live run would have failed before reaching a single
assertion.

The AWS gate is intentionally absent from the Cargo and local filesystem gates:
it creates billable resources and needs real AWS credentials, Terraform, AWS
CLI, Docker Buildx, ECR push, SSM invocation, and EC2/EBS quotas. All three
host tools are checked before anything is created, `docker info` rather than
`docker --version`, because a CLI with no reachable daemon passes the latter and
then fails at the image push with the stack already standing. The default
architecture follows the local Docker daemon so the image build is native;
`YESNO_AWS_ARCHITECTURE` and `YESNO_AWS_INSTANCE_TYPE` are explicit overrides;
the defaults are burstable `t4g.large` / `t3.large`, both Nitro — which the
source volume's NVMe-serial device resolution depends on — and both the same
2 vCPU / 8 GiB shape as the m-family they replaced. The same type backs the EKS
node group. `YESNO_AWS_EKS=0` skips the deferred EKS arm, as above.
`YESNO_AWS_TIMEOUT` bounds the whole host scenario — apply, push, provision,
three runner scenarios, destroy — and defaults to 12000 seconds. It is sized
for that whole span with slack rather than for the sum of every per-step
budget: an EKS cluster dominates it, and a Fargate task that restores an EBS
snapshot spends minutes doing so before it runs anything. A run that exceeds it
is still destroyed, by the wrapper's trap. `YESNO_AWS_KEEP=1` retains the
whole billable stack for diagnosis and must never be a CI default.

Search integration is the other scoped process exception, but not a second
harness or entry point. `e2e/search/*.py` runs through the ordinary Monty
runner and composes the shared `db_*`, `flight_*`, and `search_*` verbs.
The scenario owns the sequence and Python oracle. Host verbs invoke the
external-only `yesno-search-java` source set, install each version-locked
plugin into a freshly unpacked distribution, and return exact sorted document
IDs from a real HTTP query. Filesystem, process, download, and network authority
remain on the harness side. `scripts/gate-search.sh` selects those scenario
paths and raises their timeout inside the same all-in-one `yesno-e2e:local`
image every other containerized gate runs in. Docker is the only host
dependency.
The image contains JDK 21 and prebuilds the Java helper and both plugins. The
plugin scenarios use SHA-512-pinned OpenSearch 3.8.0 and Elasticsearch 9.5.2
archives cached in `.agents-workspace/tmp/search-e2e-cache`, with optional
environment overrides for checksum-verified local archives when the runner is
invoked directly. All engine data, logs and installed copies stay in a
disposable directory under `.agents-workspace/tmp`.

Each scenario writes keys 42 = `{1, 3, 5}` and 91 = `{3, 4}` through the
ordinary database verbs. The engine
corpus contains ordinals 0 through 6, so the assertion is a positive control:
the `yesno` queries must return exactly IDs 1, 3 and 5 and IDs 3 and 4.
Checking only a count or an empty result would not prove that coordinator
resolution reached the numeric filter.

---

## 11. What this layer cannot do

* **The remaining satellites.** `yesno-server::replication` is covered by the
  `repl_*` family in §5; the async host-call path it was once expected to need
  turned out not to be needed at all. `yesno-flight`, `yesno-arrow` and
  `yesno-datafusion` are still uncovered; the same
  block-in-the-host trick applies to Flight, and the other two are synchronous
  and cheaper still. The runner **fails loudly** on `ResolveFutures` rather than
  ignoring it, so if a verb ever does need to suspend the interpreter the gap
  will be visible rather than silent. See `e2e-network-verbs` in `JOURNAL.md` ( closed ).
* **A disk-lazy leaf.** `q_key` materializes the key through `Snapshot::load`;
  the tree above it is genuinely lazy, but the leaf is not, because the public
  API has no lazy leaf.
* **Concurrency.** One scenario, one thread. `tests/concurrency.rs` is the only
  multi-writer coverage in the tree.
* **Nanosecond wall clocks for anything but an `Expr`.** See §9.

### The AWS gate's progress output

`cloud_run` sends one Systems Manager command and polls for its terminal
status, printing nothing until it returns. With the EKS arm on that is four
silent stretches of several minutes each inside a run that already takes forty,
and the wait for the instance to register with Systems Manager is a fifth.

The harness now writes a `gate-aws:` line to **stderr** when a command is
dispatched, every 30 seconds while it is in flight, and once when it reaches a
terminal status:

```
gate-aws: waiting for i-0123456789abcdef0 to register with Systems Manager
gate-aws: e2e/aws/ebs.py sent to i-0123456789abcdef0 as 8f0e... (timeout 1200s)
gate-aws: e2e/aws/ebs.py is InProgress after 30s
gate-aws: e2e/aws/ebs.py Success with exit 0 after 352s
```

stderr rather than a `print()` in `gate.py`. A scenario's own output is
captured and shown only when it fails or when `--show-output` is passed, so a
`print()` would say nothing at the moment it is needed.

Every 30 seconds, not every poll. The poll interval is 5 s, so a
twenty-minute scenario would otherwise emit hundreds of lines and scroll the
failure that matters off the screen.

Scenario runs name themselves, because the prologue `gate.py` builds carries
`scenario=<path>`; provisioning and the Kubernetes bootstrap have no such line
and are reported as "a runner script".

### Running one arm of the AWS gate

`YESNO_AWS_ONLY=local|ecs|eks` runs that arm and skips the others. The stack,
the image and the runner are still built in full; what it buys back is the
roughly twelve minutes the two passing arms take to re-prove themselves while a
third is being debugged. `YESNO_AWS_ONLY=eks` is the fast loop on the
Kubernetes bootstrap.

**An unrecognised value is an error, not a no-op.** This is the opposite of
`YESNO_AWS_EKS`, where a typo leaves the gate testing *more*. A switch that
skips work fails the wrong way round if a typo means "match nothing": the run
would go green having executed no arm at all. `YESNO_AWS_ONLY=eks` together
with `YESNO_AWS_EKS=0` is refused for the same reason -- it would provision a
cluster, run nothing on it, and pass.

It is deliberately **not** a Terraform variable, unlike `eks`. The whole
stack is still built, so the wrapper's signal backstop cannot disagree with the
apply about what exists -- the property `eks` needs a `gate.tfvars` entry to
keep.

A partial run says so on its last line, and neither form advances TODO.md's
live deferred-materialization item:

```
gate-aws: YESNO_AWS_ONLY=eks; the local EBS arm was not run
gate-aws: YESNO_AWS_ONLY=eks; the deferred ECS arm was not run
gate-aws: PARTIAL RUN -- YESNO_AWS_ONLY=eks ran that arm and no other
```

### When the AWS gate's teardown falls short

Both automatic destroy paths — the harness's `Drop` and `gate-aws.sh`'s EXIT
trap — run in the same process tree, with the same credentials, at the end of
the same run. One expired session takes out both at once, and what stays up is
an EKS cluster billing by the hour.

`scripts/gate-aws-destroy.sh` is the escape hatch:

```
scripts/gate-aws-destroy.sh              # every run whose state still holds resources
scripts/gate-aws-destroy.sh yn-… yn-…    # exactly those runs
```

Two kinds of resource leak past `terraform destroy`, and neither is a bug in
it: the EBS CSI driver provisions a volume for a claim, and the server creates
snapshots and clone volumes for a lease. Terraform manages neither, so
destroying the cluster or the instance leaves them behind. Worse than the bill,
**one orphaned volume blocks every later run** -- the deferred scenarios open
with `aws_materializer_volumes(0, 0, 0)` and the EKS namespace is a constant,
not per-run. `scripts/gate-aws-destroy.sh --orphans` removes them, taking the
region from `AWS_REGION` or the most recent run's `gate.tfvars`. It deletes
only `available` volumes: an attached one belongs to a cluster still running.

It needs no `AWS_REGION`: each run's `gate.tfvars` carries the region and the
arm switch, and reusing that file is what stops a destroy disagreeing with its
apply about what was built. It exits non-zero and names what is left if a
destroy fails.

The trap delegates to it rather than restating the command, so there is one
definition and a failure is visible. It used to be silenced with
`>/dev/null 2>&1 || true`.

A run that owns nothing is reported and skipped rather than destroyed, because a
destroy against a run whose provider plugins were since cleaned away fails —
and reporting that as a failure for a run that owns nothing hides a real one.

**It refuses to touch a run that is still going.** The no-argument form
cannot otherwise tell an abandoned stack from one thirty seconds into
`terraform apply`, and destroying a live run is the worst thing this tool could
do. Liveness is read from `/proc`: a `terraform` holding the run's state file
shows in some process's argv, and the harness itself carries
`YESNO_AWS_STATE_DIR` in its environment even while idle waiting on Systems
Manager — the two places it spends its time.

Naming a live run explicitly is an error, because it is nearly always a
mistake; the scan skips one and says so, and then reports "nothing to destroy;
N run(s) still in flight" rather than "no run is standing", which would be a
lie.

### What a failed deferred EKS arm collects

A stalled materializer is the one failure the harness cannot explain from its
own side. The archiver reports *which* Kubernetes object it gave up on; only the
cluster says why, and `terraform destroy` follows within seconds of the failure.

When the EKS arm's scenario exits non-zero, the arm's **own cleanup trap** dumps
the archiver's namespace before it deletes anything: VolumeSnapshots,
VolumeSnapshotContents, PersistentVolumeClaims, Jobs, Pods and Events, plus the
tail of each `yesno-` pod's log.

**The placement is the whole point, and getting it wrong is silent.** Two
cleanups race the evidence: the archiver deletes its Job and claim the moment
materialization fails, and the trap's namespace delete takes the events with it.
A dump run after the scenario returned finds an empty namespace and prints six
empty sections — which reads as "the cluster had nothing to say" rather than
"we looked too late". Events outlive the objects they describe, so they are
the section that still answers "why did the pod never start" after the archiver
has cleaned up.

Reduced, not dumped. Systems Manager truncates a command's output at 24 KB
and one pod list in full would spend most of it, so each response is split on
commas and filtered to the keys that ever carry a reason — `message`, `reason`,
`readyToUse`, `phase`, `type`, `status`, `state`, `exitCode` — with `name` kept
so a message can be attributed. Not `jq`: the runner does not have it, and a
diagnostic that needs its own install is one that will not run when it is wanted.

The ECS arm passes none. Everything it could report — the task's stopped
reason, the container's exit code — the archiver already puts in the message the
scenario asserts on.

The archiver's own timeouts now carry the object's `status` as well, so a
snapshot that never became ready explains itself in the archiver log even when
no dump runs.

### Keeping a failed AWS run for a post-mortem

`YESNO_AWS_KEEP=1` leaves everything standing: the Terraform stack, the runner,
the workers, and — for EKS — the namespace with its events, its claim and its
failed pod.

**Keeping the stack alone is not enough, and that was the trap.** Each arm's
cleanup trap runs inside the runner script and removes its own leavings as it
exits: the local arm unmounts the lease clones, the ECS arm stops the Fargate
task, and the EKS arm deletes the whole namespace. Retaining the stack while
those still ran left a cluster that was billing and had nothing in it to look
at. The flag is therefore passed down into every runner script, and each trap
stands down for it.

The EKS arm still writes its failure dump under the flag. Deferring the
deletes and skipping the dump would trade one post-mortem for another.

A retained run bills until it is removed. The scenario says so on its last
lines, with the two commands that matter:

```
gate-aws: YESNO_AWS_KEEP=1; run yn-… is RETAINED and billing.
gate-aws:   reach the runner with: aws ssm start-session --target i-…
gate-aws:   cluster yesno-e2e-yn-…, kubeconfig on the runner at /etc/yesno/kube/config
gate-aws:   tear it down with: scripts/gate-aws-destroy.sh yn-…
```
