# yesno Architecture

## Repository Layout

Regenerated against the tree on 2026-08-25. The previous version had drifted
badly enough to mislead: five of the eight files in `ops/` were absent, so was
`stream/nary.rs`, three of the six crates were missing, two test layers were
missing, and `store/mod.rs` was annotated `ShardFile` — a type that does not
exist. A layout diagram is the first thing anyone reads to orient, so a stale
one costs more than no diagram.

`scripts/check-layout.py` now enforces it, and runs in the routine gate. It
checks **both** directions: a path here that is not on disk, and a
`yesno-core/src/**.rs` on disk that is not here. The second is the one that
matters — the drift above happened by *addition*, so a one-directional check
would have passed throughout. It cannot check the annotations; `ShardFile`
would still slip past it.

```text
yesno/
  Cargo.toml                    # workspace: members, shared package metadata, shared deps
  yesno-core/
    src/
      lib.rs                    # ordinal split/join, the size-class constants, and their rationale
      error.rs                  # CodecError + crate Result alias
      events.rs                 # typed storage and database lifecycle facts; no transport or serialization
      buffer.rs                 # U16Store / BitStore — the containment types for arrow_buffer
      container/
        mod.rs                  # Container enum, kind selection, promotion/demotion, optimize
        array.rs                # sorted unique u16, <= ARRAY_MAX
        bitmap.rs               # 65536-bit dense, cached exact cardinality
        run.rs                  # sorted non-overlapping non-adjacent intervals
        codec.rs                # payload encode/decode/validate, spec-byte-identical
      set.rs                    # OrdSet: struct-of-arrays chunk directory + eager set algebra
      ops/
        mod.rs                  # container-level and/or/xor/and_not entry points
        generic.rs              # the merge kernel: implementation, oracle, and safety net
        array.rs                # array x array: gallop or merge by size ratio
        bitmap.rs               # bitmap x bitmap: fused word ops with popcount
        run.rs                  # run x run: interval two-pointer
        mixed.rs                # the cross-kind arms
        card.rs                 # cardinality-only paths, no result container
        nary.rs                 # eager k-way union over one reusable scratch accumulator
      stream/
        mod.rs                  # ChunkStream / ChunkStreamExt traits
        leaf.rs                 # SetStream, EmptyStream, RangeStream
        ops.rs                  # And / Or / Xor / AndNot operators, and Not
        plan.rs                 # cost-guided Expr rewriting
        sketch.rs               # chunk-level prefix statistics for overlap
        nary.rs                 # UnionAll: lazy k-way union; Expr routes >= 3 ORs here
        dynamic.rs              # Expr + BoxedStream: runtime-constructed plans
      pack/
        mod.rs                  # Packing: where a strided object's bits live; the shared addressing
        gather.rs               # OrdSet -> canonical words: the one-pass merge; generic, and the oracle
        seek.rs                 # the seeking arm: binary search + bit-block transfer; declines rather than lies
        sink.rs                 # OrdinalSink: accumulate ordinals, one encode at build()
      matrix/
        mod.rs                  # Layout / Order / Semiring / BitMatrix; the canonical form
        read.rs                 # OrdSet -> BitMatrix: destination shape, Order, the directory popcount
        sink.rs                 # BitMatrix -> OrdSet: the scatter, one encode at build()
        gemm.rs                 # A*B + C, row-of-A driven; mul, mul_vec and pow
        elem.rs                 # elementwise add / and / complement; in-place forms are the kernels
        query.rs                # predicates, triangles, sub-matrix, outer product, pretty-print
        transpose.rs            # blocked 64x64 delta swap; the naive form is the oracle
        reduce.rs               # argmin / argmax by weight and by row; the counted product
        gf2.rs                  # Gauss-Jordan: inverse, rank, echelon, Ax = b
        lu.rs                   # PA = LU with partial pivoting; L and U packed together
      bignum/
        mod.rs                  # BigUint / IntLayout; the canonical little-endian limb form
        read.rs                 # OrdSet -> BigUint: addressability and the canonical invariant
        sink.rs                 # BigUint -> OrdSet: the scatter, one encode at build()
        addsub.rs               # add / sub / shl / shr / truncate; in-place forms are the kernels
        mul.rs                  # schoolbook, and Karatsuba above a measured crossover
        div.rs                  # Knuth Algorithm D; divrem is the only primitive
        modular.rs              # Barrett reduction and pow_mod; the precomputation is a value
        query.rs                # predicates, bit_len, byte and hex conversion
      view/
        mod.rs                  # View / ViewLayout: several OrdSets sharing one ordinal space
        select.rs               # extract and query one constituent; generic walk + aligned arm
        fold.rs                 # Reduce across constituents, and the inverse image back out
        sink.rs                 # ViewSink: pack constituents in, one encode at build()
      roaring_format.rs         # portable 32-bit format + CRoaring 64-bit map layout
      store/
        mod.rs                  # the shared geometry constants: SLAB_SIZE, SLAB_META, PAGE
        extent.rs               # ChunkKey, ChunkRef, the size-class ladder, ExtTrailer
        packed.rs               # packed pages: many small payloads sharing one slot
        alloc.rs                # generational slab + extent allocation ( I3 )
        slabmeta.rs             # per-slab occupancy, persisted in the slab's own header
        segment.rs              # the mmap'd address space — the ONLY unsafe in the crate
        superblock.rs           # A/B slots: the entire commit protocol
        checksum.rs             # hardware CRC32C, the single chokepoint
        fsck.rs                 # rebuild allocator state from the index alone
      index/
        mod.rs                  # module root: node + tree
        node.rs                 # node format, prefix-compressed leaves
        tree.rs                 # copy-on-write B+tree, bottom-up bulk build, range cursor
      wal/
        mod.rs                  # module root
        writer.rs               # framing and append; the log's base lsn; two kinds of cut
        group.rs                # group commit: many committers, one fsync
        record.rs               # 40-byte framing; lsn is the offset in one shard history
        recover.rs              # redo-only recovery, the prefix rule
      mvcc.rs                   # commit versions, the visible watermark, snapshot registry
      checkpoint.rs             # the barrier ( I4 ); the only allocator of file space
      db/
        mod.rs                  # Db / Snapshot / WriteBatch, sharded writers
        apply.rs                # one record -> memtable. Shared by recovery and the live replica
        keystream.rs            # KeyStream: one key as a lazy ChunkStream. The index scan
                                #   resolves every chunk to a ( ChunkKey, ChunkRef ); payloads
                                #   decode one at a time and counts come off card_m1, so
                                #   nothing is read that the consumer does not ask for.
                                #   The first and only leaf that reports Backing::Paged.
                                #   KeySource wraps it as a stream::ChunkSource, which is what
                                #   Snapshot::key_expr puts in an Expr -- so a query over a key
                                #   no longer materializes the key first. It answers the
                                #   planner's three leaf statistics from the index alone.
        manifest.rs             # MANIFEST: db uuid, shard count, term, and the vshard -> shard map
        readers.rs              # cross-process reader registry: a foreign reader's
                                #   ( pid, version, ckpt_seq ), plus an appended identity
                                #   region pairing each slot with the reader's process
                                #   start time, so a recycled pid no longer pins the floor.
                                #   Reclamation condition 1 is enforced from process-local
                                #   memory, so a reader in another process is invisible
                                #   without this file.
        memtable.rs             # per-shard delta with an MVCC version chain per chunk
        store.rs                # ShardStore: the assembled on-disk shard
      repl.rs                   # leader publishes raw WAL frames; follower tracks the watermark
      unstable_arrow.rs         # semver-exempt escape hatch ( policy R1 )
    tests/
      proptest_oracle.rs        # randomized properties vs BTreeSet<u64>
      differential.rs           # M0 gate: semantic + byte-level vs the `roaring` crate
      expr_equivalence.rs       # M1 gate: lazy == eager, cardinality == collect().len()
      allocation.rs             # allocation budgets asserted as tests
      events.rs                 # lifecycle ordering, correlation, panic isolation, unexpected drop
      bignum_oracle.rs          # arbitrary-precision arithmetic vs num-bigint; the OrdSet boundary
      crash_matrix.rs           # M3 gate: torn superblocks, WAL truncation at every byte
      concurrency.rs            # the only multi-writer coverage: group commit, watermark
      invariants.rs             # I2-I7 asserted directly rather than assumed
      zero_copy_mvcc.rs         # a live reader aliasing the mmap across reclamation and reopen
      durability.rs             # every read path, always after a reopen, oracle-checked
    benches/
      setops.rs                 # baselines against the `roaring` crate
      bitmatrix.rs              # bit-matrix algebra; no external reference exists — see its header
      bignum.rs                 # arbitrary-precision scaling; did not fix the crossover, see its header
    examples/
      readme.rs                 # the README's opening snippet, compiled so it cannot rot
                                # — the only example left; the measurement fixtures that
                                #   used to sit here are e2e/scenarios/*.py
    fuzz/                       # outside the workspace: needs nightly. See QUALITY_GATE §7.
      fuzz_targets/
        decode_container.rs     # decode must Err or validate, never panic
        roaring_import.rs       # the whole-file import boundary
  yesno-arrow/                  # masks, RecordBatch streams, the S4 container dump
  yesno-datafusion/             # yesno_lookup UDTF, filter lowering. Outside default-members.
  yesno-wire/                   # the set-expression wire format. Zero dependencies by
                                #   design: it is compiled into both yesno-flight ( which
                                #   decodes ) and yesno-pg ( which encodes ), and those two
                                #   resolve third-party crates through different Bazel hubs.
                                #   The interface between them is bytes, so two copies of
                                #   the code are harmless and two *implementations* are not.
  yesno-flight/                 # Arrow Flight for query results and DoPut ingest
  yesno-flight-c++/             # synchronous native Arrow C++ client; Bazel pins Arrow
  yesno-flight-go/              # Go 1.25 native Arrow Flight client; standalone module
    gate.sh                     # fmt, tidy, vet, race, and live yesnod interoperability
  yesno-tantivy/                # generation-bound Tantivy Query; optional Flight preparation
  yesno-flight-python/          # installable CPython client over PyArrow Flight; no Rust extension
    pyproject.toml              # package and tool configuration
    uv.lock                     # reproducible Python dependency resolution
    src/                        # source-layout root
      yesnodb/                  # PEP 420 namespace root; deliberately has no __init__.py
        client/                 # typed Flight client, codecs, errors, TLS/auth options, py.typed
    tests/                      # fixed wire vectors plus real yesnod interoperability
    gate.sh                     # uv lock, lint, typing, yesnod interoperability, wheel build
  yesno-flight-java/            # Java 17 Arrow Flight client under dev.yesnodb.client
    src/main/                   # typed client, expression/ticket codecs, RoaringBitmap adapters
    src/test/                   # wire vectors plus in-process Flight interoperability
  yesno-server/                 # yesnod + yesno data CLI: config, lifecycle, checkpoint driver,
                                #   durable Protobuf event journal + shared control/replication
                                #   gRPC surface on TCP and Unix listeners, TLS / Unix-peer
                                #   authentication, and ordered first-match channel/principal/
                                #   capability/source authorization rules. yesnod
                                #   owns LeaderService, FollowerClient, and the replication proto;
                                #   owns portable/ZFS/Btrfs/LVM/EBS snapshot creation, leases and cleanup;
                                #   local_transport.rs authenticates the privileged snapshot agent on the
                                #   ordinary Unix control socket, while snapshot/agent.rs brokers its work;
                                #   EBS may publish a provisional lease whose materialization is
                                #   explicitly deferred, but yesnod never launches ECS or EKS work;
                                #   a core backup barrier excludes concurrent checkpoints.
                                #   Outside default-members, so gate.sh's clippy misses it —
                                #   run `cargo clippy -p yesno-server` explicitly.
                                #   dist/ holds the deployment artifacts: systemd unit,
                                #   Dockerfile, annotated example configs, a dev-only
                                #   certificate script and two-node.sh — a runnable
                                #   failover drill. Not in gate.sh: two processes and
                                #   real ports, and the gate asserts its own step count.
  yesno-server-utils/           # administrative yesnoctl ( checkpoint, basebackup, restore )
                                #   and independently deployable yesno-archive;
                                #   library entry points are shared with the E2E host, while
                                #   binaries remain thin CLI adapters. Owns object-storage,
                                #   conditional writer fencing, immutable WAL history chains,
                                #   archive reconstruction, and deferred snapshot materialization.
                                #   An ECS task or EKS Job may restore a provisional EBS snapshot
                                #   into shared staging, while the parent archiver remains the sole
                                #   object-store writer. Consumes server snapshot leases
                                #   through Protobuf only; byte streaming is normal, same-mount
                                #   direct paths are dual opt-in. A shared client transport
                                #   makes TCP/TLS and unix:/// endpoints behave identically for
                                #   yesnoctl, basebackup, and yesno-archive.
  yesno-operator/               # kube-rs controller for the yesnodb.io/v1alpha1 YesnoCluster;
                                #   one leader plus read-serving followers, per-instance Recreate
                                #   Deployments and retained PVCs, role Services, health probes,
                                #   and a persisted fence/promote/rejoin state machine. Promotion
                                #   requires confirmed old-Pod absence and fails closed when that
                                #   Kubernetes fence cannot be established. Generates the EBS
                                #   snapshot backend per instance, resolving each volume id from
                                #   the PersistentVolume its claim bound to.
  yesno-pg/                     # PostgreSQL extension: FDW + index AM + table AM.
                                #   Outside the cargo workspace AND
                                #   built by Bazel, not cargo — a cdylib PostgreSQL dlopens is
                                #   only meaningful against one pinned server ABI. Gated by
                                #   scripts/gate-pg.sh, which gate.sh does not reach.
  yesno-c/                      # Host-independent C ABI over Db. Separate cargo workspace;
                                #   Bazel also emits its PIC static library for MySQL.
    include/yesno.h             #   ownership, threading, error-buffer, and seek contract
    tests/smoke.c               #   strict C11 ABI/runtime test against two durable databases
  yesno-mysql/                  # MySQL 8.4 storage engine; embedded C ABI or remote Flight.
    CMakeLists.txt              #   accepts Bazel inputs or adjacent-source fallbacks
  MODULE.bazel                  # bzlmod: rules_rust ( fed by Cargo.lock ), rules_foreign_cc,
                                #   toolchains_llvm for bindgen's libclang
  third_party/
    postgresql/                 # PostgreSQL built from sha256-pinned source tarballs
    mysql/                      # MySQL 8.4.0, OpenSSL, ncurses, and patchelf overlays
    arrow/                      # Arrow C++ 25.0.1 Flight build and pinned dependency inputs
    llvm/                       # libclang re-exported; see its header for the label-resolution
                                #   rule that forces the indirection
  e2e/                         # workspace-level end-to-end assets
    aws/                        # opt-in real-AWS EBS gate: Terraform for resources, four scenarios
    filesystems/                # opt-in ZFS/Btrfs/LVM + deployed archive/S3 scenarios
    mysql/                      # hermetic MySQL scenario, SQL, and expected fixture
    scenarios/                   # operational sequences and measurement fixtures
    operator/                    # opt-in kind scenario driven by the ordinary Monty runner
    postgresql/                  # hermetic PG scenario, SQL, expected, and isolation fixtures
  yesno-e2e/                    # Python scenarios run by `monty` against a real Db
    BUILD.bazel                 # Bazel-facing ordinary runner; no server/DataFusion graph
    src/
      world.rs                  # handle tables, dispatch, and the database verbs
      eager.rs                  # sb_* / set_* / ct_* / ops_*: OrdSets and kernels, no Db
      lazy.rs                   # q_* / st_*: the planner, and raw ChunkStream cursors
      matrix.rs                 # mx_*: BitMatrix values, and the OrdSet boundary
      bignum.rs                 # bn_*: BigUint values; Python's own int is the oracle
      flight.rs                 # flight_*: the shipped Flight service over a loopback socket
      fixture.rs                # fx_*: resources, subprocesses, sessions, diagnostics, cleanup
      operator.rs               # op_* kind lifecycle verbs, observations, diagnostics, cleanup
      filesystems.rs            # fs_*: KVM, ZFS/Btrfs/LVM, Winterbaume S3, deployed archive/restore
      aws.rs                    # aws_*: processes, RPCs and tagged-resource counts inside the EC2 runner,
                                #   for the local-mount and both deferred arms of the EBS backend
      cloud.rs                  # cloud_*: the host intrinsics that put a scenario on that runner
      server.rs                 # srv_*: yesnod plus base-backup/archive/restore sequences
      convert.rs                # MontyObject <-> yesno, and per-verb argument checking
  scripts/
    gate.sh                     # the routine gate; --deep adds Valgrind, ASan, TSan
    gate-operator.sh            # opt-in Docker/kind/kubectl operator lifecycle gate
    gate-filesystems.sh         # opt-in KVM ZFS/Btrfs/LVM archive and Winterbaume S3 gate
    miri.sh                     # withdrawn 2026-08-29; kept runnable, gated by nothing
    valgrind.sh                 # the UB gate; the mmap sites and both unsafe blocks
```

## Data Model

An ordinal is a `u64`, split as:

```text
ordinal = (prefix48 << 16) | low16
```

`Prefix48` is a `u64` with the invariant `< 1 << 48`. `CHUNK_BITS = 16` and `CHUNK_CARD = 65536` are fixed by the `u16` container value width and by the resulting 8 KiB bitmap being L1-resident. They are not tuning knobs.

`OrdSet` is a **structure of arrays**:

```rust
pub struct OrdSet {
    prefixes: Vec<Prefix48>,
    containers: Vec<Container>,
    len: u64,
}
```

The parallel vectors mirror the on-disk chunk directory. Every merge-join and every `seek` binary-searches `prefixes` alone, so keeping it dense means about 8 entries per cache line with container payloads never touched during search. `len` is maintained incrementally so `OrdSet::len()` is O(1).

### Container Size Classes

| Kind | Payload | Bound |
|------|---------|-------|
| Array | `2 * card` bytes of sorted unique LE `u16` | `card <= ARRAY_MAX` ( 4096 ) |
| Bitmap | exactly `BITMAP_BYTES` ( 8192 ), LSB-first LE | always 8192 |
| Run | `2 + 4 * nruns` bytes: `nruns` LE `u16`, then `(start, len_minus_1)` pairs | `nruns <= RUN_MAX_INTERVALS` ( 2032 ) on write, `RUN_DECODE_MAX` ( 32768 ) on read |

Three constants exist to stop pathological churn, and each has a stated reason in `lib.rs`:

- `ARRAY_MAX = 4096` — a full array and a bitmap are the same payload size ( `4096 * 2 == 8192` ), which is what makes array → bitmap promotion a same-class rewrite.
- `BITMAP_DEMOTE = 3584` — demotion happens *below* 4096, not at it. The 512-value gap is deliberate, asymmetric hysteresis: alternating insert/remove at exactly the boundary would otherwise cost an 8 KiB conversion per operation. Bitmaps are cheap to hold, so being lazy about demotion is free; being late about promotion is not an option.
- `OPT_GAIN_NUM / OPT_GAIN_DEN = 7/8` — `optimize()` switches encodings only on a >= 12.5% byte saving, so a container cannot oscillate between encodings on every commit.

`RUN_MAX_INTERVALS = 2032` is a capacity choice for our size classes ( `2 + 4*2032 = 8130 <= 8192` ), not a spec limit; decode accepts up to 32768 intervals so foreign CRoaring files stay readable.

## Buffer Containment Policy ( R1 )

`buffer.rs` holds the containment types, `U16Store` and `BitStore`, and everything that touches a *container payload* goes through them.

**This section used to open "`buffer.rs` is the only module that names `arrow_buffer` types", and that is false.** Eleven do: `buffer`, `checkpoint`, `container::codec`, `db::store`, `index::tree`, `lib`, `store::alloc`, `store::fsck`, `store::segment`, `stream`, `unstable_arrow`.

Most are internal and harmless, and naming an arrow type *inside* the crate has never been what R1 forbids — it constrains the **public API**, which is what a semver bump is measured against.

**R1 is now true, and checked. `scripts/check-r1.py`'s baseline reached zero on 2026-08-28.** It started at six and every entry left for a different reason, which is worth recording because only two were the refactor the policy anticipated:

* **Three were never violations.** `buffer.rs` is `pub(crate) mod`, so a `pub fn` inside it is unreachable from outside the crate. The checker could not see that until `module_is_public` walked the ancestor declarations rather than reading the item's own `pub`.
* **One was dead code** — `store::segment::check_alignment`, deleted rather than refactored, having had no caller outside its own tests.
* **`index::tree::NodeReader::node`** was closed as anticipated, by returning an opaque `Page` in the spirit of `U16Store` / `BitStore`.
* **`store::segment::buffer_at`** was closed by making `store::segment` `pub(crate)`. Nothing outside `yesno-core/src` names `SegmentedMmap`, so the type was never public API in intent — only in declaration.

**Making that module private surfaced three items that `pub` had been masking**: `MmapSegment::{len, is_empty}` and `ExtentGuard::{cell, cell()}`, all with no caller anywhere. `pub` in a public module suppresses dead-code analysis, so **module privacy is itself an unwired-code detector** — and the one place this crate had not pointed one. `ExtentGuard::seg` is *not* in that set: it is deliberately never read, carries `#[allow(dead_code)]` and an explanation, and deleting it would compile cleanly and dangle the pointer Arrow holds.

The design's R7 ( `cargo semver-checks` against the last published core ) remains the mechanical enforcement, and still needs a published baseline to diff against.

Both stores are lifetime-free. The `Shared` arm is a refcounted buffer that may alias an mmap, and that is precisely the property that makes `Container: 'static + Send + Sync` — which in turn is what lets streams be boxed, stored in a struct, and sent across threads.

Copy-on-write is explicit: a container decoded from a page is a *slice* of the segment buffer, so `Buffer::into_mutable()` would fail on it regardless of alignment. `to_mut` / `words_mut` therefore copy on every mutation of shared data by design, which is the semantics immutable published extents require.

## Set Algebra

There are three layers, and they answer different questions.

### 1. Container kernels — `ops/`

`ops::and` / `or` / `xor` / `and_not` take two `&Container` and return
`Option<Container>` ( `None` meaning the result is empty ). `ops::apply`
dispatches through the measured array, bitmap, run, and mixed arms before
falling back to `ops::generic`, and all nine ordered kind pairs now have
specialized materializing paths. The cardinality and relation operations in
`ops::card` form a separate dispatch table; a materializing specialization
does not accelerate them automatically.

The generic sorted merge remains simultaneously:

1. the differential-test oracle for every specialized path,
2. the safety net when an arm declines a shape or alignment,
3. the initial implementation for any operation that has not yet justified specialization.

**Specialize an arm only when a benchmark demands it, and keep the generic
kernel reachable and correct forever.** Every materializing change must audit
`ops::card`, `is_disjoint`, and `contains_all` separately.

### 2. Cardinality identities — `ops/card.rs`

All four cardinalities reduce to `and_cardinality` plus cached lengths:

```text
|A ∩ B| = and_card(A, B)
|A ∪ B| = |A| + |B| − and_card(A, B)
|A ⊕ B| = |A| + |B| − 2·and_card(A, B)
|A \ B| = |A| − and_card(A, B)
```

One non-allocating kernel family therefore buys four zero-allocation queries. This is why `Container::len()` must stay O(1) on every representation, and why `BitmapContainer` maintains its cached `len` incrementally on every mutation rather than recounting.

### 3. Streams — `stream/`

`ChunkStream` yields `(Prefix48, Container)` in strictly ascending prefix order and never yields an empty container. The trait has no lifetime parameter and no GAT, because `Container` is `'static + Clone + Send`; a borrowed `Container<'a>` design would make a boxed, stored, cross-thread plan node unrepresentable.

Four contract points matter when touching this module:

- **`next_chunk` returns `Result` even though in-memory leaves cannot fail.** Once leaves decode from an mmap'd page store they can, and retrofitting fallibility through every operator later would be a breaking change.
- **`peek_prefix` is a lower bound, not a promise.** XOR and ANDNOT may report a prefix whose chunk then cancels to empty and is skipped. Use it for ordering decisions only. Anything needing an exact answer ( emptiness, cardinality ) must go through `next_chunk` or `cardinality_dyn`.
- **Operators buffer one chunk; leaves do not.** An operator's `peek_prefix` cannot be answered from its children's prefixes alone, so each operator keeps a one-slot lookahead filled by `produce`, making its own `peek_prefix` exact. Leaves peek by reading the prefix array without touching a payload, which is what keeps seek-driven AND cheap where it matters.
- **Every operator must override `cardinality_dyn`.** The default materializes a container per chunk. Without the overrides, the `Box<dyn ChunkStream>` path silently falls back to the slow version and the cardinality identities buy nothing. `tests/allocation.rs` exists to catch exactly that decay.

`ChunkStreamExt` holds the combinators so that `ChunkStream` itself stays object-safe. `Expr` ( in `dynamic.rs` ) is the runtime-constructed tree a query planner builds when it does not know the shape ahead of time; `Expr::open()` lowers it to a `Box<dyn ChunkStream>`.

### Unary NOT

`OrdSet::not_in_range( lo, hi )`, `ChunkStreamExt::not_in_range( lo, hi )` and `Expr::not_in( lo, hi )` complement within a half-open range; `not()` on each complements over the whole universe.

The unbounded form exists **because of I8**. Without a reserved top value the complement of the empty set would have cardinality `2^64`, which no `u64` can report, so `not()` could not have a correct signature even in principle. Reserving `u64::MAX` also makes `[0, u64::MAX)` name the entire universe, so the exclusive bound never needs the unrepresentable `2^64`, and the crate's two range conventions line up: an inclusive bound tops out at `ORDINAL_MAX`, an exclusive one at `u64::MAX`.

**There is no eager `OrdSet::not()`, and there was for about an hour.** An eager complement materializes its whole answer, and over the full universe that is ~`2^48` chunks — so a no-argument `not()` aborts for *every* input, including a nearly-full set whose complement is tiny, because the walk is `2^48` either way. It was shipped and documented as "complete rather than practical", which is a rationalisation rather than a design. Removed.

The lazy form has no such problem: `!expr` and `ChunkStreamExt::not` yield one chunk at a time and count in `O(chunks of the input)`. Measured on a 5 000-chunk set: `(!e).cardinality()` in 92 µs, first chunk in 33 µs, against an abort for the eager form. `not_in_range` stays unbounded-by-request — a `2^40`-wide range really does produce 16.7 million chunks in about a second — but that cost is visible in the arguments, which is the difference.

#### `Not` is a real operator, and was not

It shipped first as `pub type Not<S> = AndNot<RangeStream, S>`, on the reasoning that `NOT x == Range \ x` is an exact identity and a second operator would duplicate `AndNot`'s lookahead, seek discipline and cardinality override. That reasoning was sound for a **bounded** complement and is wrong for an unbounded one:

> `AndNot::cardinality_dyn` drives its loop from the **left** operand. With `RangeStream` on the left that is one step per chunk *of the range* — fine for a narrow range, and ~10^14 steps for the whole universe.

So `x.not().cardinality()` was a hang, not a slow path. `Not` now owns its loop and overrides cardinality with

```text
|[lo, hi) \ S|  ==  (hi - lo) - |S ∩ [lo, hi)|
```

which walks **`S`'s** chunks instead. Measured: **zero allocations** counting a complement of ~131 million ordinals, against 1.00 per chunk for the old formulation, and 479 µs for an unbounded complement over a 5 000-chunk input.

`Expr::Not` is a variant for exactly the same reason. It was sugar lowering to `AndNot( Range, x )`, which reintroduced the `2^48` walk through the dynamic path even after the operator was fixed — the two decisions had to move together, and the second was found only because a test asserted on elapsed time.

The kernel is still the shared `and_not`; only the driver changed. The bitmap-complement specialization ( word-wise `!w` ) remains untaken per QG §4 — and `and_not( run, bitmap )` is the better place to spend it, since that pays here *and* on every existing ANDNOT.

`Expr::open` **flattens a contiguous `Or` subtree** and routes three or more leaves through `stream::nary::UnionAll`, the shared scratch accumulator, rather than folding pairwise. The threshold of three is the design's and the measurement agrees: over 400 chunks, k=2 is `5 -> 4` allocations ( 1.6x ), k=3 is `409 -> 6` ( 3.4x ), k=8 is `2 429 -> 11` ( 7.9x ). Until this was wired, `UnionAll` was reachable from no query at all — written, unit-tested, and folded past by the only entry point that could have used it.

### The planner — `stream/plan.rs`

`Expr::open` plans before lowering. It exists because the cost of an expression depended on **which of several equivalent spellings the caller used**, and the spread was not subtle. Measured against a 5 000-chunk set, before the planner:

```text
  And(x, Range)        352 µs      AndNot(Range, x)     > 5 s
  AndNot(x, Range)     472 µs      Or(x, Range)         > 5 s
  !x                   199 µs      Xor(x, Range)        > 5 s
```

`AndNot(Range, x)` *is* `!x` longhand and `Xor(x, Range)` is the same complement again, so the three slow cases were not inherently costly — they just missed the operator that drives its loop from the input. Asking callers to know which spelling is fast is not a design.

**Cost model.** Two numbers, and the distinction between them is the point:

- `yield_chunks( e )` — chunks the stream produces when drained.
- `cardinality_cost( e )` — chunks visited to answer `cardinality()`.

They differ for exactly two nodes, and those two are why the planner pays: a `Range` counts by subtraction ( `O(1)` however wide ), and a `Not` counts by `(hi - lo) - |input ∩ range|`, so it walks **the input**. A complement that yields `2^48` chunks is counted in a few thousand steps.

**The cost is read from the operands, not from the shape.** `Set` reports its real `chunk_count`, `Range` its real width, so the same rewrite is accepted for one dataset and declined for another — `de_morgan_is_declined_when_the_data_says_it_is_worse` pins a case where narrowing the range and enlarging the inputs reverses the decision. A structurally-decided rule would be wrong in one direction or the other.

**Rules.** Unconditional: empty-identity folding; `AndNot( Range, x ) -> Not( x )` ( the definition of a complement ). Conditional on containment, decided by `bounds()`: `And( x, R ) -> x`, `Or( x, R ) -> R`, `Xor( x, R ) -> Not( x )`, `AndNot( x, R ) -> Empty`. Conditional on provable disjointness: `And -> Empty`, `AndNot( a, b ) -> a`, `Xor -> Or`. Range algebra: intersection and union **fuse**, and difference **splits** into up to two range leaves — the one rewrite that grows the tree, and still cheaper because both pieces count arithmetically. Cost-guided: both De Morgan directions, applied only when `cardinality_cost` strictly drops.

**Planning must stay cheap relative to execution, and once did not.** The fixpoint check was `format!( "{next:?}" ) == format!( "{cur:?}" )`, and `Debug` on `Expr::Set` renders the entire set — so every operand was serialized twice per pass. On a realistic query ( a 1 000-chunk dense operand ) that made `plan()` take **192 ms for something that executes in 37 µs**: the planner costing five thousand times the work it was saving, while the doc claimed it was memoized. `same_shape` compares set leaves by `Arc::ptr_eq`; planning is now ~2 µs against 24 µs of execution. Anything added to the planner needs measuring against the query it plans, not just against correctness.

**Termination** does not rest on the loop cap. Every cost-guided rewrite strictly decreases a non-negative integer, and the rest strictly shrink the tree; the cap is a backstop against a future rule pair that oscillates.

**Overlap — `stream/sketch.rs`.** An interval cannot express "occupies alternating chunks", so `bounds()` alone costed `And` as `min( yield_a, yield_b )` however much the operands actually intersected, and could never prove two interleaved sets disjoint. Chunk-level statistics answer both:

- **`Set` against `Set` is exact**, by galloping merge of the two sorted prefix arrays. Cheaper than the intersection it is costing — `u64` compares over the dense prefix array, no payload access — and admits no false zero, which is what licenses rewriting to `Empty` on its strength.
- **Composite sub-expressions get a bottom-`K` sketch** ( `K = 256` ). `splitmix64` is a bijection on `u64`, so distinct prefixes never collide and the sketch is **exact whenever it holds every prefix**; above that it degrades to the usual KMV estimate. Disjointness is claimed *only* from an exact source — a saturated sketch declines rather than guesses, because the planner turns a `true` into `Expr::Empty` and a false positive would silently delete rows.

`And` now costs the estimated shared prefixes and `Or` / `Xor` the estimated union, so cost tracks how much two operands really share rather than how large they are: against a 1 000-chunk operand, `And` costs 1000 / 100 / 0 for identical / lightly-overlapping / disjoint partners.

**A Bloom-style bitset was tried first and is wrong.** Equal prefixes hash equal, so a zero AND does imply disjointness — but by the birthday bound 500 prefixes in a 4096-bit filter collide on ~61 bits, so the AND is essentially never zero and nothing is ever proved. Sizing it properly needs bits proportional to `n²`. Do not re-add it; the unit test that killed it is `disjointness_is_claimed_only_from_a_complete_sketch`.

### Split and concatenate — simplifying interleaved operands

A union of operands whose chunk ranges only partly overlap was charged for a full merge across the **whole** domain, even where a region has one contributor. `Or` peeks both sides once per prefix, so `Or( small_set, huge_range )` costs a step per chunk of the range and does not finish — to produce a result that is, over most of its extent, simply one stream after the other.

**Segmentation** cuts the prefix domain at every point where the contributing set changes — the operands' span endpoints — so within a segment that set is constant. Each segment is then either **one contributor** ( a pass-through, no merge, and its `cardinality` is the operand's own: `O(1)` for a range ) or **several** ( a real merge, confined to the region that needs it ). Segments are ordered and disjoint by construction, so `Concat` reassembles them with no comparisons at all.

Split and concatenate are inverses, which is what makes the rewrite obviously meaning-preserving: every chunk lands in exactly one segment. Measured against a 3 000-chunk set:

```text
  Or( small, huge disjoint Range )        > 5 s  ->  610 µs
  Xor( small, huge disjoint Range )       > 5 s  ->  999 µs
  Or( small, huge partial overlap )       > 5 s  ->  751 µs
```

Two operators support it. `Concat` requires its operands to be prefix-disjoint **and ordered** — violating that yields chunks out of order, which every operator above silently mis-merges, so it is selected only from the opened streams' reported spans and carries a `debug_assert`. `Restrict` clips a stream to a prefix window, and **delegates `cardinality_dyn` when the window does not actually clip** — without that the segmentation built the right plan and then executed it the slow way, walking `2^48` chunks to count a segment that contained a whole operand.

Segmentation declines when it cannot pay: unknown spans, identical spans ( every segment has every contributor, so there is nothing to save ), or an operand too expensive to re-open. Re-opening is the cost model here — a segment opens each contributor separately, which is free for a leaf and would duplicate a whole evaluation for a composite subtree, so it is restricted to `Set` / `Range` / `Empty` operands.

**Occupancy refines what spans can only bound.** A span is a min and a max, so an operand whose chunks are clustered at both ends is credited with the whole gap between them, and every segment there is merged against an operand that has no chunk in it. `PrefixOccupancy` records, per operand, which coarse buckets of the prefix domain it actually occupies.

Note this is *not* what the bottom-`K` sketch does. That answers **how much** two operands overlap; segmentation needs **where**, and a bottom-`K` sketch cannot say — it holds hashes, and `mix` being a bijection is precisely what destroys the ordering the question depends on. Positional questions need positional buckets.

Occupancy is **exact at its resolution**: a bucket is `(prefix - base) >> shift`, so a bucket is marked iff the operand really has a chunk in it, with no collisions and no false positives. The resolution loss is one-directional in the safe way — dropping an operand from a segment requires that *no* overlapping bucket is marked, which means it genuinely has no chunk there, while including one that contributes nothing merely merges unnecessarily. Unlike disjointness, a coarse answer costs optimization and never correctness.

**The bucket count is a cost bound, not a precision knob.** Refining to single-prefix resolution would separate two operands on alternating chunks perfectly — into `2n` segments, each re-opening its operands, which is far worse than the merge it replaces. Capping at `OCCUPANCY_BUCKETS` ( 256 ) bounds the segment count by construction, so truly interleaved operands come back as "both, everywhere" and are left alone. That is the right answer for them. What occupancy finds is **gaps**, not interleaving.

Two invariants hold this together, both stated at their sites: every occupancy transition is a cut point, so an operand's occupancy is **uniform within a segment**; and occupancy is required for *every* operand rather than best-effort per operand, so there is no "this one has no statistics" branch on the path that drops contributors — that branch was unreachable in every test, and inverting it dropped operands wholesale unnoticed.

### Chunk profiles — what a run of chunks *is*

Occupancy answers "are there chunks here". That is not enough, because the algebra depends on their character: a **1-filled** run is an identity for `∩` and an annihilator for `∪`, and a **0-filled** run is the reverse. `ChunkProfile` run-length-encodes an operand's prefix domain as `Empty` / `Full` / `Present`, computed from `Container::is_full()` — a cached-length comparison, so it touches no payload.

It is exact, and its size is the number of runs rather than a fixed budget: a `Range` is at most three runs however wide ( partial, full, partial ), and a uniformly sparse set is one. Past `MAX_PROFILE_RUNS` it coarsens to a single `Present` run, which claims nothing and so loses optimization rather than correctness.

The payoff is that **an operand which is 1-filled over a region is a range literal over that region**, and every rule that recognizes a range now recognizes it too. The absorption rules were written against `Expr::Range`; `covers( outer, inner )` generalizes them to "outer is `Full` across inner's whole prefix span", of which a range is the special case. So `x ∩ A`, `x ∪ A`, `x \ A` all collapse for a 1-filled `A` without a payload being read.

`of_range` is derived from the definition — chunk `p` is full iff the range covers all of `[p << 16, (p+1) << 16)` — and not from "the ends are partial, the middle is full". That shortcut marked chunk 0 of `Range( 0, 40 )` as `Full`, which would have let `covers` conclude that 40 ordinals contain a whole 65 536-ordinal chunk: an **unsound** absorption, not a missed one.

### Static rewriting is only half — `ChunkStream::stats`

Everything above rewrites an `Expr` **before** anything is open, reading statistics off `Expr::Set` leaves. That works today only because every leaf happens to be a materialized `OrdSet`: `Snapshot::load` materializes before streaming. Two things it structurally cannot do:

- see behind a `Box<dyn ChunkStream>` handed in from elsewhere, which has no `Expr` at all;
- distinguish a chunk that is a pointer dereference from one that is a page fault. A `Paged` chunk under a 2 MiB folio reads 256x what it needs, and the static cost model counts it as 1.

So the plan has to be finished **dynamically, once the operands are open and it is known what stands behind them**. `ChunkStream::stats() -> StreamStats` is that channel: remaining `chunks`, `prefix_span`, and a `Backing` ( `Computed` < `Memory` < `Paged` < `Unknown` ) carrying a relative per-chunk cost. It is deliberately distinct from `cardinality_hint`, which bounds *ordinals*; this reports *chunks and what produces them*, which is what decides how much touching a stream costs.

Three properties make it usable rather than decorative:

- **It describes the remainder, not the original.** A partially-consumed stream reports what is left, so a decision taken mid-execution sees the truth.
- **It survives type erasure.** `BoxedStream` delegates. Without that the default `unknown()` applies the moment a stream is boxed — and every stream a planner touches is boxed. This is the same trap `cardinality_dyn` fell into once; both are sabotage-tested.
- **A stream that declines to report is charged pessimistically**, so silence never wins a comparison against an operand that answered.

The first consumer is the n-ary union decision. It used to be `parts.len() >= 3` — a property of the *expression*, right on average and wrong whenever the operands are not what the shape suggests. Three streams of two chunks each do not need an 8 KiB scratch accumulator. `Expr::open` now opens the children first and decides from their reported volume.

**No leaf reports `Paged` yet**, because nothing streams from pages. The variant exists because it is the reason the channel is needed at all, and `stats_drive_the_plan_not_just_chunk_counts` builds a stream that reports it, so the path is exercised rather than aspirational.

**Soundness** is checked by `planning_preserves_meaning` in `tests/expr_equivalence.rs`: planned against unplanned against the `BTreeSet` oracle, over random trees containing `Range` and `Not` nodes. 52 of 256 generated trees are actually rewritten — asserted, because a planner that rewrites nothing preserves meaning perfectly and the property would pass vacuously. Dropping the containment guard from any absorption rule fails it.

### Strided packing — `pack/`

Where a strided object's bits live in the ordinal space, and the two transfers that move them. `matrix/` and `bignum/` are both built on it; neither owns a copy of the walk any more.

A `Packing { line_bits, lines, line_stride, object_stride }` says that bit `b` of line `l` of object `k` sits at ordinal `k*object_stride + l*line_stride + b`. That is `matrix::Layout` minus `Order`, and it is exactly `bignum::IntLayout` at `lines == 1`. Each lens keeps its own type — the field names, the `check()` messages and the constructors are domain vocabulary — and converts with `packing()`.

**Three `u64` axes, and they are independent.** A db **key** says which set; an **ordinal** is a member of it; an **object index** `k` addresses a strided range of ordinals *within* one set. A packing is about the third and knows nothing about the first.

**Why this is one module and not two copies.** The two lenses were written independently and the second reached for the first as a template, so `bignum/read.rs`'s gather was `matrix/read.rs`'s gather at one line — the same `partition_point_in` / `chunk_at` / ascending-merge walk, with the same two bounds. The cost of that was not the duplicated lines: it was that **`matrix/` had a seeking arm and `bignum/` did not**, and the arm is entirely layout-generic. Merging delivered it to `bignum` without writing it.

**`gather` is the oracle; `try_gather` is the arm.** Same contract as `ops::generic` — the arm **declines rather than lies** ( it returns `false` for a bitmap whose buffer is not 8-byte aligned, which an mmap-backed container legitimately is ), and the generic path is never deleted because the arm is faster.

**One pass, because two would be quadratic.** The gather merges two ascending sequences — the source ordinals and the lines' ordinal ranges — in a single walk. Written the obvious way, as a gather per line, it is `O(lines × container size)`: **5.5 ms to move 8 KiB**, about 1.5 MB/s. Do not reintroduce it.

### 4. Bit matrices — `matrix/`

A `u64` ordinal set is a bit vector, and a bit vector under a `Layout` is a series of dense M×N boolean matrices. The chunk arithmetic lines up: 65536 bits is 1024 `u64`, so 8×8 is one word, 64×64 is 64 words, and 256×256 is exactly one container. An `OrdSet` may hold many matrices; every operation names one.

**The seam is paid at the boundary, never in a kernel — and the boundary is now `pack/`.** `M` and `N` are arbitrary and `Layout` carries independent strides for lines and matrices, so a row can start at any bit offset and a matrix can straddle a chunk. Rather than putting shift-and-carry into the product, the transpose, the inverse and every reduction, `pack/gather.rs` fills a **canonical** form — row-major, rows padded to whole words — and `matrix/sink.rs` scatters back. The kernels between them are branch-free word loops.

**`Order` does not reach `pack/`, and must not.** `Layout::line_len` and `Layout::line_count` normalize row-major and column-major to "lines of bits" before `Layout::packing()` is taken, so a `ColMajor` source is gathered into the transpose and `read_matrix` transposes afterwards. A `Packing` carrying an order would be describing the *reading*, which belongs to the lens.

That is also what makes `M * N < 65536` a real specialization axis: a matrix inside one chunk under an already-canonical layout is a `memcpy`. But `M * N < 65536` does **not** imply chunk containment — a 100×100 matrix is 10 000 bits, so matrix 6 spans 60 000..70 000 and crosses the boundary. Straddling follows from `matrix_stride`. A fast path must test containment, not infer it, and a generator that omits a straddling index leaves the seam untested while every property still passes.

**The generic reader is the oracle**, on the same terms as `ops::generic`: it handles every stride, both orders and the straddle, it ships first, and it is not deleted when a specialized reader is faster.

**Dense value in, dense value out.** Results of this algebra are dense by construction, so encoding one into containers means choosing a representation and running `Container::optimize`, which re-selects by serialized size. Doing that per operation would pay it for intermediates nothing reads — so chained work stays in `BitMatrix` and `MatrixSink::build` encodes once.

**Semirings are a closed enum**, following `SetOp` and for a stronger reason than precedent: on packed bits the only additive monoids available are `|` and `^`. `&`'s identity is all-ones and `\` is not associative, so the set really is closed at two.

### What this module is not

Measured against [nessan/gf2](https://nessan.github.io/gf2/), a general GF(2) library, `matrix/` carries the overlap that matters: GEMM, matrix × vector, `pow`, transpose, inverse, rank, echelon and reduced echelon, `LU`, `Ax = b`, the elementwise algebra, the triangles, sub-matrix, outer product and the predicate set.

**Four things are deliberately absent**, recorded here so the decision is not re-taken by accident:

- **Characteristic polynomial, Frobenius form, eigen-structure.** That library's headline feature, and a computer-algebra system. This is an index.
- **`companion` / shift / rotation constructors.** LFSR shapes with no caller here.
- **`random` / `biased_random`.** `CLAUDE.md` puts measurement code outside `src/`.
- **`resize` / `append_row` / `remove_col`.** A `BitMatrix` is a dense value read out of a set, not a growable container.

The structural difference underneath all four: `gf2`'s matrix *is* the object, so its API is the algebra. Ours is a **view over an ordinal set** with two semirings, so it also carries `read_matrix` / `MatrixSink` / `counted_mul` and the argmin-argmax family, which that library has no analogue for.

### 5. Big integers — `bignum/`

The same reinterpretation as `matrix/`, at a different shape. A `u64` ordinal set is a bit vector, and a bit vector under an `IntLayout` is a series of arbitrary-precision unsigned integers: bit `j` of integer `k` lives at ordinal `k*stride + j`, carrying the `2^j` term. An `OrdSet` may hold many integers; every operation names one.

**Least significant bit first, and there is deliberately no `Order` knob.** Under `IntLayout::dense` the ordinal set and the integer series are the same object, which buys three identities for free: `OrdSet::or` on operands with no bit in common **is** their sum, `OrdSet::xor` is addition in `GF(2)[x]`, and — the decisive one — reading a stored integer under a **narrower** `width_bits` yields exactly `x mod 2^width_bits`. A most-significant-bit-first option would reverse every gather, and under it the value of every stored bit would depend on the width, so there would be no such identity to test.

**The seam is paid at the boundary, never in a kernel — and the boundary is now `pack/`.** `pack/gather.rs` fills the canonical form — a little-endian `u64` limb vector with no trailing zero limb — and `bignum/sink.rs` scatters back. Everything between them walks limbs and carries only arithmetic carries. A file under `bignum/` other than `read.rs` and `sink.rs` that names `IntLayout`, `width_bits`, `stride`, `CHUNK_CARD` or `split()` is in the wrong file, and a file under `bignum/` that names `Packing` at all other than `mod.rs` and `read.rs` is too.

**An integer is a `Packing` with one line, and that is what let the two lenses merge.** A one-line packing's canonical words *are* a limb vector, so `bignum/read.rs` and `matrix/read.rs` now share one gather and one seeking arm rather than a copy each. The property that says the abstraction is real — reading `k` as a `1 × W` matrix equals reading it as a `W`-bit integer — is `a_one_line_packing_reads_the_same_as_a_single_row_matrix`, and it could not be written before.

**`width_bits < 65536` does not imply chunk containment**, the same trap as `matrix/`'s and with the same numbers: at `stride = 10 000`, integer 6 spans 60 000..70 000. `IntLayout::straddles` exists so a caller can test the claim rather than re-derive it, and `IntLayout::chunk_aligned` is the spelling that avoids it. **The sharpening this module adds: the straddling unit is the limb, not the integer.** 65 536 is a multiple of 64, so under a stride that is not, a single *limb* can cross the boundary — integer 655 of `dense(100)` begins at bit 65 500 and its first limb spans 65 500..65 564. A reader that copied whole limbs per chunk would be wrong for exactly one limb, and every test whose width is a multiple of 64 would still pass.

**A repeated index is refused, which deliberately diverges from `MatrixSink`.** `MatrixSink::place` unions a repeat, and that is defensible there because OR-ing two boolean matrices *is* `Semiring::Boolean` addition. OR-ing two integers is not addition, not maximum, and nothing a caller could have meant — 5 then 3 at one index yields 7. `IntSink::place` therefore requires strictly increasing indices, which makes a repeat impossible to express at `O(1)` cost and with no memory of what has been placed.

**The ceiling test is on the span, not on the top set bit.** Admitting a placement because its highest set bit happens to be low would make acceptance depend on the *value* rather than the layout, so the same index would take 1 and refuse `2^(width_bits-1)`.

### What this module is not

**Not a cryptographic library, and it will not be hardened one function at a time.** Nothing here is constant-time. The arms dispatch on limb count, division branches on operand values, and stripping leading zero limbs makes even a value's length data-dependent — so timing leaks the magnitude before a kernel runs. This is an index; there is no key material and no adversary with a timer. A partially constant-time module is worse than an honestly variable-time one, because it invites the assumption that the rest is too.

**Unsigned only.** `sub` returns `None` below zero, following `invert_gf2` on a singular matrix: the answer is not in the domain, and that is a fact about the operands rather than an error. Not a wrapping subtraction — there is no width to wrap to, because the value type deliberately carries no width. Width is a property of `IntLayout` and of nothing else, which is what keeps `mul` and `divrem` from needing a width argument.

**No `std::ops` implementations.** `Sub`, `Div` and `Rem` cannot be total here, so they would have to panic beside an `Option`-returning `sub`. Two spellings of one operation, one of which aborts the process, is worse than one honest spelling.

**No bitwise `and` / `or` / `xor` on `BigUint`.** Under a dense layout `OrdSet::and` / `or` / `xor` already *are* those operations — that is the identity above — so adding them would be a second implementation of shipped set algebra.

**No Toom-3 and no Burnikel-Ziegler, and that was measured rather than deferred.** The multiply ladder stops at Karatsuba and division at Knuth D. Toom-3 wins nothing below 168-264 limbs across every plausible linear constant, and Burnikel-Ziegler's floor puts its break-even near 512; a chunk is 1024 limbs and nothing in the crate produces operands near that. Falsifiable, with the condition written down: past ~256 limbs build Toom-3, past ~512 build Burnikel-Ziegler. And if that day comes, **division is the better target** — `div / mul` at equal size grew from 2.3x at 64 limbs to 4.6x at 1024, because Karatsuba pulled `mul` sub-quadratic while Knuth D stayed `O(m*n)`. See JOURNAL for the tables.

**No Montgomery form, no GCD, no modular inverse, no FFT multiply.** Montgomery needs an odd modulus and a general `pow_mod` cannot assume its caller's parity, so `Barrett` is the generic path and Montgomery is a later specialized arm under QG §4. If it lands it must be a **separate type**: Montgomery changes what a `BigUint` operand *means*, and a `mul_mod` that sometimes expects entered operands and sometimes ordinary ones returns a well-formed wrong answer — the packed-`L`-and-`U` trap with no escape hatch. GCD and modular inverse need signed intermediates, a representation this module deliberately does not have.

**Three branches in `bignum/` are unreachable by random testing**, each fires for roughly `2^-64` of operands, and each is the *sole* coverage of a real correction step: Algorithm D's D6 add-back, its D3 `qhat >= 2^64` clamp, and Barrett's negative-correction branch. All three carry a **constructed, checked-in corpus and a counter the test asserts is non-zero**, plus a complement test showing that sampling reaches none of them. Treat the corpora like `tests/*.proptest-regressions` — they only grow, and deleting an entry silently removes a branch's only test.

### 6. Views — `view/`

Several `OrdSet`s packed into **one** ordinal space. A `View { sets, layout }` maps `( constituent, logical ordinal )` to a physical ordinal and back; nothing is stored, and a `View` is a descriptor the caller constructs exactly as `matrix::Layout` and `bignum::IntLayout` are.

**A view is an `n × W` boolean matrix whose rows are the constituents**, and the two layouts are that matrix's two orders — `Interleaved` ( `x*n + i`, column-major ) and `Blocked` ( `i*stride + x`, row-major ). They are transposes, which is why the same three questions have opposite costs under them:

| operation | `Interleaved` | `Blocked` ( aligned ) | measured |
|---|---|---|---|
| extract one constituent | 3.25 ms | **179 ns** | `view/select` |
| cardinality of one | 2.08 ms | **35 ns** | `view/cardinality` |
| membership in one | 8.3 ns | 9.7 ns | equal, as designed |
| build ( place all `n` ) | 19.3 ms | 4.5 ms | `view/build` |

**Measured, not derived** — `benches/view.rs`, 4 constituents over 200 000 logical ordinals, on a heavily loaded machine so read the *ratios* rather than the absolutes. The asymmetry is far larger than the original prose guessed: extraction is ~18 000x and cardinality ~59 000x, because the aligned blocked arm is a prefix relabel with payloads shared by refcount and `len_in_range` probes at most two chunks, while interleaved must walk every ordinal. A **non-aligned** blocked stride loses the arm entirely and measures 729 µs — 4 000x the aligned case — so `Blocked`'s advantage is a property of the stride, not of the layout.

**This is deliberately not `matrix/`.** That module reads a *dense value* out of a set — bounded, materialised, in memory. A view over 8 constituents of `2^40` ordinals is `2^43` bits, a terabyte dense, so every operation here works on the sparse representation. `matrix/` is used as a **differential oracle** at sizes that do fit, and `a_small_view_is_a_bit_matrix_whose_rows_are_the_constituents` is that test.

**Elementwise algebra across two views is free, and it is a theorem rather than an optimization.** `docs/formal-model.md` §15.2 proves the map from a set to its object stack is a bijection under a dense layout, so for two sets packed under the same view `OrdSet::and` / `or` / `xor` **are** the `n`-wise elementwise operations at once, through no new code. There is deliberately no `view_and`; `View::compatible_with` exists instead, because a mismatched pair produces a well-formed *wrong* answer rather than an error and nothing in the type system can catch it.

**The one case that is nearly free.** Under `Blocked` with a stride that is a multiple of 65 536, a constituent is a whole number of chunks and its logical ordinals differ from its physical ones by a multiple of the chunk width — so the low 16 bits are unchanged, every container **is** the answer, and extraction is a prefix relabel with payloads shared by refcount. `O(chunks)`, no payload access. The generic walk remains the oracle and the two are diffed.

**`range_summary` is not used here and should not be.** It computes the full count and compares with no short-circuit, so "is anything in this slot" costs "how many are in it"; and probing per slot is *quadratic* on a bitmap chunk, since `Container::rank` is `O(v/64)`. Both paths walk once instead. See the `is_range_empty` item in `TODO.md`.

**Build memory is `O(packed cardinality)`** — `ViewSink` accumulates through `pack::OrdinalSink`, 8 bytes per ordinal placed. Unlike `MatrixSink` and `IntSink` this is *not* bounded by the descriptor, because a constituent is an arbitrary `OrdSet`. The mirror of the aligned select arm would fix it; not built, awaiting a measurement.

**Not in `pack/`.** `pack/mod.rs`'s header says a packing is an *injection* and that nothing there maps several ordinals onto one. That stays true: `view/` is the consumer that does.

**Name collision**: `aligned-chunk-views` in `TODO.md` uses "view" for an unrelated three-valued `Empty`/`Full`/`Present` leaf classification in the evaluator. Different layer, same word.

**The fold, and the algebra that constrains it.** `view_fold( v, Reduce )` reduces the `n` constituents to one set over the shared logical ordinals — `Any` is their union, `All` their intersection, `Parity` their symmetric difference. Closed at three for the reason `matrix::Semiring` is closed at two, and one walk serves all three: it counts, and each variant reads its answer off the count.

**A fold is the opposite of a restriction, and this is the trap.** §14 Proposition 18 lets a prefix window push down with **no** side condition; a fold pushes through nothing unconditionally. Each monoid is exact for exactly one operator — `Any` for `∪`, `All` for `∩`, `Parity` for `△` — and nothing commutes with `\`. A reader who has internalized Prop 18 will assume otherwise. `fold_of_an_intersection_is_contained_and_sometimes_strictly` pins the one-sided law *and* asserts the inclusion is sometimes strict, so it cannot pass on an accidentally-exact implementation.

**`view_expand` is the free direction.** The inverse image is a homomorphism of the *whole* signature — inverse images always are — so it distributes over `∩`, `∪`, `△` and `\` alike, and `∃ ⊣ ⁻¹ ⊣ ∀` is an adjoint triple. That is why `expand( coarse ).and( fine )` composes with everything while `fold( a.and( b ) )` does not. Both directions are pinned together by `expand_and_fold_form_an_adjunction`.

**The fold generalizes §7.1.** The planner's occupancy abstraction is `Reduce::Any` at stride 1 on the prefix axis, and its Proposition 9 ( sound omission ) is the `∩` one-sided law. A fold is a caller-declared zone map, sound in exactly the same direction: it can prove disjointness, never non-emptiness.

**Arms, and one of them has to decline.** Selecting each constituent and combining with ordinary set algebra is the oracle — correct everywhere, and it reuses the tuned pairwise kernels rather than reimplementing them. Under `Interleaved` that is `O( n · nnz )` because each select is itself a strided filter, so a single grouped walk replaces it at `O( nnz )`; it is correct there because the physical order *is* the logical order, which is exactly what fails under `Blocked` where `x` restarts per constituent. The interleaved fold arm measures **5.1x** the oracle ( 3.42 ms against 17.5 ms ).

**`view_expand`'s interval arm is not uniformly better, and shipping it unconditionally was a regression.** A logical ordinal's `n` slots are contiguous under `Interleaved`, so adjacent inputs coalesce and a 20 000-ordinal range becomes two chunks of run containers rather than 80 000 values — 2x the generic path. But a *scattered* input coalesces into nothing, so the builder makes one `insert_range` per input ordinal where the generic path makes one bulk sorted build, and it measured **9.1x slower**. That is the crate's oldest measured asymmetry in disguise ( bulk build against per-ordinal insert, 133 µs against 3.70 ms ). The arm now runs an `O(nnz)` allocation-free counting pre-pass and declines when the expansion will not coalesce; sparse input went 5.92 ms -> 500 µs, within 8% of the generic path. `EXPAND_INTERVAL_COST` is the measured cost ratio behind that test and is not a tuning knob.

**Still no core `Expr` variant and no descriptor catalog.** Flight carries `ViewSpec` in `ViewSelect`, `ViewFold`, and `ViewExpand` wire nodes and evaluates each transform eagerly before re-entering the Boolean tree as a set leaf. The packed bits persist under an ordinary database key; the caller must supply the same descriptor on every request. A lazy planner node and a server-side `view_id -> descriptor` catalog remain separate, measured design choices.

## Serialization — `roaring_format.rs`

Because container payloads are already spec-identical, reading and writing a `.roaring` file is `O(container count)`, not `O(cardinality)`.

Two subtleties are centralized rather than open-coded:

- **The offset-header rule.** The 32-bit format's `u32` offset array is present **iff** the cookie is `SERIAL_COOKIE_NO_RUNCONTAINER`, **or** the cookie is `SERIAL_COOKIE` and there are at least `NO_OFFSET_THRESHOLD` ( 4 ) containers. Getting this wrong silently misparses small run-encoded bitmaps, so it lives in `has_offsets` and is tested directly.
- **Which 64-bit layout.** Two incompatible 64-bit layouts exist in the wild. We implement CRoaring's `Roaring64Map`: a `u64` bucket count, then per bucket a `u32` high key followed by a complete 32-bit portable bitmap. Java's `Roaring64NavigableMap` is *not* supported.

## Error Model

`CodecError` ( `error.rs` ) is the single error type, with `Result<T, E = CodecError>` as the crate alias. Its variants describe how a byte range failed to become a container: `BadLength`, `BadCardinality`, `BadRunCount`, `OutOfBounds`, `Misaligned`, `UnknownKind`, `UnsupportedEncoding`, `Invariant`, `Truncated`, `BadCookie`.

`container::codec::decode` is a fuzz target by contract: for *any* input it must return one of these errors or a container satisfying its invariants, and must never panic.

## Storage Engine — `store/`

Everything below is on-disk state for one shard. The full rationale for each decision lives in the module-level `//!` comments; this section is the map and the numbers, so that a change knows what it is constrained by before it opens a file.

**Slab occupancy is persisted in the region each slab reserves** ( `store/slabmeta.rs`, written before the superblock flip ). It is a **cache of derivable state** — `fsck::rebuild` can recompute all of it from the index — which is what makes an in-place write legal there: a torn block decodes to `None`, the slab stays `Opaque`, and nothing is lost but the ability to reuse it.

**Slab 0 is reserved and never allocated into.** Its `SLAB_META` region *is* the two superblock slots, so it cannot record occupancy; a reopened shard restored it as `Opaque`, which silently disabled reclamation and identity verification for anything in it. Reserving it costs no blocks — `grow_to` extends with `set_len`, so an untouched body is a hole — and removed a small unbounded leak, since chunks that landed there could never be reclaimed after a reopen.

**Slab 0 has no metadata region.** `SLAB_META` is 8192 and the reserved superblock prefix is also 8192 at offset 0, so slab 0's notional metadata region *is* the two superblock slots. `slabmeta::offset_of` returns `None` for it. Writing there destroys the A/B redundancy while leaving every test green, because the flip that follows rewrites one slot — see JOURNAL, 2026-08-25.

**A reopened shard must account for the slabs already on disk.** `Allocator::new()` has no slabs, and `new_slab_for` pushes at `slabs.len()` — so an allocator that does not know about existing slabs hands out slab 0 and writes over live extents. `ShardStore::open` uses `Allocator::reopened(sb.n_slabs)`, which marks them `Opaque`: never allocated into, and never assumed free. Slab occupancy is not persisted yet ( see `persist-slab-metadata` ), so "unknown" is the only honest state for them.

**There is exactly one allocator during a checkpoint.** Extents and index nodes both draw from it, through `CheckpointSink: ExtentWriter + NodeWriter + AllocSource`. This is stated as an invariant because violating it is silent: a second allocator is not corrupt, it simply knows nothing, and hands out space that is already in use. Do not pass an `Allocator` alongside a sink, and do not `mem::take` one out of a store to satisfy a borrow — that is the exact shape that made every checkpoint overwrite its own extents with index nodes ( JOURNAL, 2026-08-25 ).

**The keystone consequence.** Containers alias the mmap directly, so a write to a page a live snapshot can reach is *undefined behaviour*, not a torn read. That is what upgrades shadow paging from a preference to a requirement, and it is why several rules below are absolute rather than advisory.

### Layout

| Constant | Value | Where | Why it is that value |
|---|---|---|---|
| `SLAB_SIZE` | 2 MiB | `store/mod.rs` | Equal to the PMD size, so no extent straddles a huge-page folio. |
| `SEGMENT_SIZE` | 1 GiB | `segment.rs` | A multiple of `SLAB_SIZE`, so no extent straddles a mapping boundary and one `Buffer` never needs two guards. |
| `INDEX_NODE` | 1 KiB | `store/mod.rs` | Node size sets *write* amplification under COW, not read latency. 4 KiB rewrote ~4 KiB of index per ~5 B of payload change. |
| `PACK_MAX` | 2028 | `extent.rs` | Below the clustered-payload cliff. Tail-waste is flat across 512–1014, so the choice is free there; 2028 is what avoids a 17% regression when payloads cluster near 1310 B. |
| `CHUNKKEY_BYTES` | 14 | `extent.rs` | A `ChunkKey` is 112 significant bits. Storing it as 16 wastes two bytes on *every* index entry. |
| `RECLAIM_CKPT_DELAY` | 2 | `alloc.rs` | The A/B superblock still names the N−1 root. Same two-transaction delay as LMDB. |
| `COMPACT_LIVE_FRACTION` | 0.40 | `alloc.rs` | `spaceAmp = ln(1/C)/(1−C)`, `writeAmp = 1/(1−C)`. Product-optimal is 0.285; the curve is sharp on the high side, so err low. |

### The size-class ladder

Eleven classes ( `MAX_CLASSES`, and the superblock's class array ends where `OFF_ROOT` begins — adding one is a format change, not a tweak ). Every entry is ≡ 0 mod 64, and slab bases are 2 MiB-aligned, so **every slot in every class is 64-byte aligned by construction**. Bitmap alignment is therefore a property of the ladder rather than a per-kind rule an allocator change could silently break.

Two properties to preserve when editing `CLASS_SIZES`:

- **8256 is exact-fit for a bitmap** ( 8192 + 8-byte trailer ) — zero internal fragmentation for the class that dominates dense data.
- That same class also holds a full 4096-element array, so **`Array(4096) → Bitmap` promotion stays a same-class rewrite** with no slab migration. This is the hottest conversion in the system; segregating heaps by container kind would turn it into a cross-heap move, which is the main reason grouping is by *size class* and not by kind.

The ladder is persisted in the superblock, not compiled in. Retuning it is therefore not a format break, and a file written by a differently-tuned binary stays readable.

### Packed pages

Arrays and runs with payload ≤ `PACK_MAX` share a page rather than each taking a slot; bitmaps are never packed. `ChunkRef.cell` points *directly* at a payload — no per-chunk directory, no length field — because length is derivable from the reference alone. The page carries no `live_bytes` counter and is **immutable after write**, which makes it structurally incapable of violating I2 rather than merely careful about it.

**The checkpoint must `prune` before it evicts.** `evict_durable` only drops a chain reduced to a single durable version, and `prune` is the only thing that reduces one. Without it, `delete_key` followed by `insert_many` leaves `[value, tombstone]` for ever — never evicted, permanently dirty, and rewritten on every later checkpoint along with its extent. Measured at 2.57x aged amplification against 1.14x.

**A checkpoint costs the delta, not the database.** Untouched chunks carry by *reference* — `run` takes their existing `ChunkRef` and merges it into the rebuilt index without reading or rewriting a payload — and `Memtable::evict_durable` drops chunks the store can now answer, so they stop counting as dirty. Both are needed: with only the first, everything stayed in the memtable and nothing was ever untouched.

**Eviction is bounded by the oldest pinned index root, not by `safe_version`.** A `Snapshot` pins the roots live at its creation — the *last checkpoint's* — while its version is the current `visible`, so anything committed in between exists only in the memtable. Evicting that state makes the key vanish for that one reader while staying present for everyone else. `Db::evict_floor` tracks the root watermarks; `safe_version` is a different quantity and using it here is a silent data-loss bug ( JOURNAL, 2026-08-25 ).

**Reading at a named version is a first-class request, and it can be refused.** `Db::snapshot_at( v )` registers a reader at `v` rather than at `visible`, which is what lets N endpoints answer one query from one instant — a Flight ticket carries the version for exactly this. The two calls above are also what *ends* a version's readability: after `prune( f )` a read below `f` sees the newest surviving entry rather than its own, and after `evict_durable( f )` it falls through to a root carrying the checkpoint watermark. `DbInner::read_floor` records how far that has gone and `snapshot_at` refuses below it. Refusing rather than answering from the floor is the load-bearing choice: the floor's state is a *plausible* answer that nobody downstream can distinguish from the right one, so a coordinator would assemble a union that never existed and report success. And the order inside `snapshot_at` is not incidental — claim the slot, *then* read the floor. Claiming drags `safe_version` down to `v` so no later checkpoint can prune past it; checking first reopens the check-then-act window this exists to close.

## Chunk Index — `index/`

A single global copy-on-write B+tree over `ChunkKey = (key << 48) | prefix48`. An ordered scan of every chunk for one key is the range `[key << 48, (key+1) << 48)` — one `u128` compare per probe.

`Tree::build_reusing` keeps leaves whose entries are unchanged, so a small edit rewrites a few nodes rather than the whole tree ( measured: 3 against 50 ). Two rules make that work and both are load-bearing: reuse requires the `ChunkRef`s to match, not just the keys — a moved extent must not reuse a leaf naming the old one — and a rebuilt range is split to end **exactly** on the previous leaf boundary, or an inserted entry overflows the leaf and misaligns every boundary after it.

Superseded index pages are reclaimed through the `NodeSpace` sink capability. The free set is `old_nodes - reused_nodes`, **never** `old_nodes` — reuse keeps unchanged leaves reachable from the new root, so freeing the whole old tree queues live pages. A live `Snapshot` reading a pinned older root is protected by the version rule rather than by this exclusion: pages are queued at the current watermark, and `safe_version` cannot exceed any live reader's version.

Reads merge an in-memory delta over a B+tree cursor at the snapshot root; the tree is rewritten only at checkpoint, bottom-up. That is "LSM with exactly one in-memory L0 and one on-disk level" — the write batching without the scan read-amplification.

Two things not to reintroduce:

- **No leaf sibling pointers.** Under COW a sibling link is invalidated by the sibling's own rewrite, forcing cascading updates. Range scans use a cursor stack instead.
- **No per-key chunk directory.** A single global tree is what dissolves the "one key's chunk list exceeds memory" problem — a huge key is just a long cursor walk. A per-key directory would need two-level paging to bound it.

A leaf entry is `ksuf_len + 8` bytes: a truncated key suffix plus the 8-byte `ChunkRef`. Because that reference carries `card_m1`, `len(key)` and `is_empty(key)` are answerable from an index scan **without touching a payload extent**, which is what makes the cardinality identities cheap and feeds DataFusion statistics directly.

**Keys are enumerable as of 2026-08-29, and the codebase used to say they were not.** `Flight::list_flights` answered "the key space is a `u64`, not an enumerable catalogue", which was true of the *space* and never of the *contents*: `ChunkKey` packs the key in its high bits, so one key's chunks are a contiguous B+tree range and the populated subset was always walkable. `Snapshot::{keys, key_range}` walk it at **O( distinct keys )** — each key is found and then *seeked past* via `ChunkKey::range_end`, so a key holding 100 000 chunks costs one descent rather than 100 000 steps. Guarded by `tests/allocation.rs`, not by a correctness test: a scan-through returns the identical list, and measured 11 248 allocations against a budget of 160 when sabotaged. A key present only as a tombstone is **absent** — reporting it would resurrect a deleted key for every caller that enumerates.

`card_m1` also carries the **range** questions, which is what the DataFusion pushdown is built on. `Snapshot::{len_in_range, range_summary}` scan the leaf entries for the chunks a range touches and decode a payload only for the at most **two** the range covers *partially* — every chunk it covers wholly answers from the number already in its leaf. So deciding `skip` / `scan` / `scan_selection` for a 1 M-row row group, about sixteen chunks, costs two container reads whatever the range's width. Guarded by `tests/allocation.rs`, not by a correctness test: an implementation that decoded every chunk returns the identical number, and measured 3 201 allocations against 388 chunks when sabotaged.

**Half-open `[lo, hi)`**, unlike `Db::insert_range`'s inclusive `[lo, hi]`. Both conventions exist on purpose and neither is smoothed over; the half-open one is what a row group's `[start, start + count)` already is. `RangeSummary` is `Empty | Full | Partial` and `#[non_exhaustive]` — `Full` is the answer worth having, because it is what lets a row group be scanned with **no** selection vector at all, and a count alone cannot tell it from a large `Partial`.

## Arrow edge — `yesno-arrow`

Three paths, and they are not equally good. **Masks** are the point: a bitmap container *is* a `BooleanBuffer`, bit for bit, so a posting list becomes a row filter for a refcount bump. **Containers** ( S4 ) ship the payloads themselves, byte-identical to the page store's and to a `.roaring` file's, so a dump is a copy and reading one back is `O( container count )`. **Ordinals** materialize `u64`s and are the escape hatch — reach for them when the consumer genuinely needs integers.

Three cost properties hold here that no correctness test can see, so all three are pinned by allocation counts:

- **An absent chunk is free.** A mask stream over a contiguous ordinal space emits a mask per chunk whether or not the set has anything there, so over a sparse set most masks are gaps. `BooleanBuffer::new_unset` allocates and zeroes 8 KiB *per call*; `empty_mask` hands out clones of one process-wide zero buffer instead. Measured at 401 allocations for 200 gaps when sabotaged. Do not "simplify" it back to `new_unset`.
- **A batch costs a batch, not a container.** `OrdinalBatchReader` carries a `Container` plus a `DecodeCursor`, not the container's decoded ordinals — which would be 512 KiB of `u64` to emit an 8 192-row batch. The cursor exists because a `ChunkStream` hands out `Chunk<'_>` and a borrowed iterator cannot outlive it: owning the container is a refcount bump, but an owned iterator over it would be self-referential, so the position is carried instead. `Container::fill_from` peels bitmap words with `trailing_zeros` and keeps the unconsumed bits in the cursor.
- **A dump is a copy.** S4 payloads come from `codec::encode` — the same function the store and the `.roaring` writer use, because a second encoder drifts. `kind` and `cardinality` are columns because neither is derivable from the payload alone: an array of `n` values and a run of `n / 2` intervals occupy the same bytes.

There are **no nulls anywhere**, structurally: every field is `nullable = false` and every array is built with `nulls: None`. A posting list is a set of present values, and absence is already a zero bit — a validity buffer would double the mask allocation and destroy the zero-copy handoff.

Stated here, in the README and in `ChunkRef::cardinality`'s own doc — and **not implemented** until 2026-08-25. `Snapshot::cardinality` summed `merged_chunks(key)`, decoding every container first: 2 129 allocations for a 500-chunk key, against 122 for the index walk. No correctness test could see it, since both return the same number. Now guarded by `cardinality_is_answered_from_the_index_not_by_materializing` in `tests/allocation.rs`. A property this document calls headline needs a test that fails when it stops holding, or it is a wish.

## WAL, Checkpointing, and Recovery

Within one shard, `lsn` is the record's global byte offset, not a counter — that makes a cursor seekable and a shipped byte range offset-identical on a follower. `term` is in the 40-byte header from day one; retrofitting it would be a format break.

**The commit time is in a body, not the header, and the asymmetry with `term` is deliberate.** A `ShardCommit` or `Abort` carries eight bytes of UNIX-epoch microseconds, flagged by bit `0x01` of the header's previously unused `flags` byte. `term` is stamped on every record, so only a header field could hold it; a commit time belongs to a *commit*, and every commit necessarily writes a resolving marker whose body was empty. The cost is therefore eight bytes per commit per participating shard rather than eight per record — which matters because the format is shaped around `SetRange`, a 25-byte body that makes bulk load affordable, and widening the header would tax every one of those to carry a value identical across the whole batch. Three further properties fall out and are the reason this placement was chosen over widening the header or adding a record type: the header, its offsets and its CRC coverage are untouched, so the byte-exhaustive crash matrix keeps its offset-dependent assertions; a log written before stamping still reads, because an empty marker body was already legal; and a log written *after* it still reads on an older binary, because the record type is known and nothing in recovery inspected that body. The other half of that bargain is that an unknown `flags` bit is an **error**, not something to skip — ignoring one would let a later writer's body be read as an earlier writer's.

The checkpoint also persists the commit clock's high-water mark in the superblock. A checkpoint is exactly the point at which the WAL carrying those stamps stops being replayed, so without it a restart immediately after one resumes the clock from the system clock alone, and a clock that stepped backwards in between would stamp new commits under old ones. See **I9**.

**"Global" means across the shard's whole history, not within the current file, and until 2026-08-28 it meant the second.** Checkpoints then emptied one file and later records restarted from file offset zero, so a shard reused every LSN it had ever issued. A follower's cursor stopped meaning anything at the first checkpoint and could be answered with "you are caught up" while the replica silently stopped advancing.

Each shard WAL is now a sequence of generations. `shard-NNNN.wal` is active;
a checkpoint seals it as `shard-NNNN.wal.<20-digit-base-lsn>` and creates a new
active file at the sealed generation's end. Sealed files are immutable and
retention deletes them only as whole generations. Thus
`lsn = generation_base + file_offset`, and the logical history stays contiguous
without moving surviving bytes. The first frame makes a non-empty generation
self-describing; an empty active generation takes its base from the newest
sealed end, or from `SuperBlock::wal_replay_lsn` when none remains.

Recovery truncation and checkpoint rollover are different operations.
`truncate_to` removes an invalid suffix, even when it begins inside a sealed
generation. A checkpoint seals the active generation and then reclaims complete
generations below the retention floor. Do not collapse these back into a single
length mutation.

The physical end captured while sealing a WAL generation is not necessarily the
replay-safe reclamation cutoff. A commit may have appended durable bytes while
its version remains above the sampled visible watermark. The checkpoint records
the first LSN above its watermark and preserves the generation containing that
record; reclaiming through the physical seal end could discard recovery input.

A follower below the oldest retained generation still has one remedy —
bootstrap again — and gets it as a `FailedPrecondition` naming that.

**Checkpointing needs no full-page writes.** Extents and index nodes are always written where no published root can reach them, so a half-written one is unreachable. The only in-place writes are the slab free bitmaps and slab table, both CRC-protected and both re-derivable by scanning the index — so a torn write there costs a rebuild, not data. That was aspirational until 2026-08-25: `fsck::rebuild` marked only the slots reached from *chunk* references, and index **nodes** occupy allocator slots too, so adopting its output would have marked every live node free and the allocator would have written chunk payloads over the tree. The rebuild now walks `Tree::node_ids` as well, which is what makes the sentence true and what `Db::open` relies on. The commit point is the superblock flip, which does not require the device to write a sector atomically.

**Recovery is redo-only.** By I4 nothing from an unresolved commit version ever reaches the data file, so "undo" is just "don't redo". A commit version is committed iff every shard named in its `CommitIntent` has a CRC-valid `ShardCommit`. No acknowledged commit is ever discarded, because a commit is acked only after `visible >= cv`, which requires every version at or below it to have been resolved and fsynced.

## MVCC — `mvcc.rs`, `db/`

Keys hash to one of `VSHARDS = 256` **virtual** shards. All chunks of one key live in one shard ( I7 ).

The `vshard -> shard` map is **persisted**, in an A/B'd `MANIFEST` beside the shard files that also carries the database identity and the shard count.

It was `vshard_of(key) % shards.len()` until 2026-08-28, and the entry describing that called it "harmless in v1, where the shard count is fixed at open". Nothing fixed it: reopening a 4-shard database with 8 returned **31 of 64 keys**, with 3 returned 14 — silently, because the count was part of the routing function and the request was never checked against what was there. The count now comes from the MANIFEST and `DbOptions::shards` is a *creation* parameter.

There are three routing sites, not one — `Db::shard_of`, `WriteBatch::commit`, and `Snapshot::shard_index` — and all three must go through the map. Fixing only the accessor left the write and read paths recomputing the modulo for a day. Two sites computing the *same wrong* answer round-trip perfectly, so no read-back can see it; the test observes which shard's WAL actually grew.

Commit versions are assigned **late** — inside the participating shard locks, taken in ascending order — which gives per-shard monotonicity ( I5 ) and shrinks the assign-to-durable window to the fsync. Readers snapshot `visible`, never `next`, so a partially durable batch is invisible rather than half-visible. That is what allows the shard lock to be released before the fsync, which is what makes group commit effective.

**Extent reclamation requires three conditions, and none is redundant:**

1. No active snapshot can reach it through any retained root.
2. `current_checkpoint >= freeing_checkpoint + RECLAIM_CKPT_DELAY` — the A/B superblock rule.
3. No live Arrow `Buffer` points into it.

Condition 1 covers *entitled but not yet read*: a snapshot may be allowed an extent it has not materialized, where the refcount is zero at that instant. Condition 3 covers *read and escaped*: `Buffer` is `'static`, so a `RecordBatch` can outlive the `Snapshot` that produced it. Neither implies the other.

**Condition 1 is `safe_version > obsolete_at`, strictly, and the boundary is not an off-by-one.** The tempting reading is that a reader at `V == obsolete_at` sees the *new* copy so cannot need the old one. That is true of what it reads through the *current* root and irrelevant: `Snapshot` captures `roots` at creation, so a snapshot taken just before the superseding checkpoint has `V == W` **and** pins the older root, which still points at the old extent. Version alone cannot separate it from one created after the flip.

Loosening it to `>=` was tried on 2026-08-25 and is unsound. It does not fail loudly — the generational allocator rarely re-hands-out a single freed slot, so the corruption stays latent until the slab empties and is recycled whole, and no sanitizer sees it because it is a use-after-free of a *file slot*. Gating on the oldest pinned root instead ( `evict_floor() >= obsolete_at` ) is unsound for a second reason: a checkpoint does not prune the memtable, so consecutive checkpoints with no commits between them supersede extents at the **same** `obsolete_at`. The precise condition is about checkpoint *sequence numbers* and needs per-reader state that does not exist; see `stale-root-blocks-idle-reclamation`.

Condition 3 is answered by `SegmentedMmap::any_pinned_in`, a per-cell `Weak<ExtentGuard>` registry. It is a **range** query on purpose: a packed page is freed at its base cell while live readers point at payload offsets inside it, so an exact-match check would call a busy page reclaimable.

An emptied slab — one whose last slot passed all three conditions — is recycled by `new_slab_for` rather than the file being extended. Partially used slabs are **not** scavenged: that is the compactor's job, and doing it on the write path would forfeit the locality generations exist for.

Slabs below `COMPACT_LIVE_FRACTION` are evacuated at checkpoint — their live chunks rewritten into the current generation so the slab empties and can be recycled — rate-limited to `EVACUATE_PER_CHECKPOINT` so a checkpoint never becomes a whole-database rewrite.

**A generation does not abandon a partly-filled slab.** `begin_generation` bumps the counter and keeps the active slabs. Clearing them — which the generational rationale reads as implying — costs one 2 MiB slab per class per checkpoint however little it wrote, measured at 12-15x aged space amplification, and compaction cannot recover it because relocated chunks land in the slab abandoned next. The bulk-load locality the rationale is actually about is unaffected: a bulk load is one checkpoint and still claims a contiguous run. Gated by `aged_space_amplification_stays_bounded`.

Growth is reduced, not bounded; bounding file *size* needs slab evacuation. **Nothing in slab 0 is ever reclaimable.** It has no metadata region ( the region is the superblock ), so a reopened shard restores it as `Opaque` and `owning_class` refuses everything in it. A test small enough to fit in slab 0 therefore cannot exercise reclamation at all, which is what hid condition 3 being untested for three milestones.

This paragraph used to continue *"`begin_generation` clears the active-slab table each checkpoint, so bump allocation always opens a fresh slab and a freed slot is never reallocated. Condition 3 is therefore implemented and unit-tested but not yet load-bearing end-to-end."* **Every clause of that is now false**, and it contradicted the paragraph immediately above it — which is the more reliable signal that a document has drifted than any single claim. `begin_generation` keeps its active slabs ( the measurement that changed it is quoted there ), freed slots *are* reallocated, and condition 3 is load-bearing. Left visible rather than deleted, because the failure mode is instructive: the correction was written next to the stale text instead of over it, and both then read as authoritative.

The deferred free list needs **no persistence at all**, which follows from I3 + I4: extents allocated since the last checkpoint are unreachable from any durable root, so a crash simply loses them. This depends entirely on I3 — do not weaken it.

That argument used to continue *"and on restart there are no readers, so the checkpointed free bitmaps already describe exactly what is reclaimable. No durable structure, no orphan scan, no leak."* **The bitmaps do not.** An extent superseded but not yet past all three conditions still holds its slot, and the checkpoint persists that slot as *used* — so after a reopen nothing referenced it and nothing had it queued, and it was lost for good, once per restart. Measured as `pending: 3` before a reopen becoming `leaked: 3` after.

The claim is true of the **index**, not of the bitmaps. `Db::open` therefore walks it and adopts the result ( `ShardStore::rebuild_allocator_at_open`, behind `DbOptions::rebuild_alloc_on_open` ). That is safe *only* at open — one root, no readers, an empty deferred queue, so "unreachable from the committed root" and "free" are the same set — and it refuses on any doubt, because adopting an incomplete liveness map is not a lost repair but data loss.

## Snapshot Leases and Archive Materialization

A base snapshot is a server-owned immutable object with a renewable lifetime,
not an archive object and not an MVCC `Snapshot`. `yesnod` is the only process
allowed to touch the live database path or create and delete the leased capture.
The client receives either a flat file manifest or a provisional EBS
descriptor, but never ownership of that capture itself. Deferred workers may
create transient restored volumes, which remain archiver-owned materialization
resources rather than server snapshot objects.

There are two independent leases. The **snapshot lease** keeps the server's
immutable provider object alive while it is read. The **archive writer lease**
fences publication of object-store state. Holding either one grants none of the
authority of the other:

```text
                checkpoint / base request
                         |
                         v
  +---------------+  BeginBaseSnapshot  +------------------+
  | yesno-archive | -------------------> |      yesnod      |
  |               |                      |                  |
  | archive writer|                      | snapshot lease   |
  | lease + object|                      | + live data path |
  | publication   |                      | + provider owner |
  +-------+-------+                      +---------+--------+
          |                                        |
          |                          backup barrier | capture
          |                                        v
          |                               immutable provider object
          |                                        |
          |  file manifest OR provisional EBS     |
          | <--------------------------------------+
          |
          +--> read or materialize --> validate/recover --> publish base
          |
          +--> ReleaseBaseSnapshot -------------------------> delete object
```

The backup barrier is deliberately a **capture barrier**, not a transfer
barrier. WAL commits continue while it is held, but checkpoints do not. The
portable provider copies its bounded database file set under the barrier. ZFS
and Btrfs create their read-only filesystem snapshots there. LVM verifies and
flushes the configured origin, then creates a classic copy-on-write snapshot LV
there. EBS verifies and flushes the source and receives `CreateSnapshot` there.
Mounting an LVM snapshot, waiting for an EBS snapshot, restoring a volume,
copying files, and uploading bytes all happen after the barrier has been
released. An LVM or EBS point in time may cut a WAL record; ordinary recovery
of the immutable clone supplies the consistency boundary before publication.

The provider result selects one of three consumption paths:

```text
                              immutable capture
                                     |
                 +-------------------+-------------------+
                 |                   |                   |
                 v                   v                   v
       portable / ZFS /       local EBS          deferred EBS
        Btrfs / LVM lease      restored clone     provisional lease
                 |                   |                   |
          flat file manifest    flat file manifest    snapshot ID,
                 |                   |                region, size,
        +--------+--------+          |              fs and subpath
        |                 |          |                   |
   Protobuf chunks   dual-opt-in     |          yesno-archive launches
                     direct path     |          ECS task or EKS Job
        |                 |          |                   |
        +-----------------+----------+-----------+-------+
                                                   |
                                            shared staging
                                                   |
                                                   v
                                  parent archiver publishes objects
```

LVM is strictly a server-local, file-bearing provider. `yesnod` owns its
namespace, lease table, and barrier timing, but a second local process owns all
privileged execution. The configured
`source_mount` is the mount root of one linear origin LV and the database may
be at a subpath below it. Capture runs `lvcreate --snapshot` with a fixed COW
allocation, then materialization mounts the snapshot read-write for filesystem
journal recovery and remounts it read-only before enumerating the bounded
database files. Release and expiry unmount before `lvremove`; restart
reconciliation selects only names in the database UUID namespace whose LVM
origin matches the configured LV. The snapshot mount tree must resolve outside
the live origin mount. An LVM lease never enters the ECS/EKS deferred path.

The independently deployed `yesno-snapshot-agent` uses the same Unix gRPC
control socket as archive and operator clients. Its one-byte prelude carries
explicit Linux `SCM_CREDENTIALS` with the process's real PID, UID, and GID;
the server enables `SO_PASSCRED` and requires those credentials to agree with
the socket's `SO_PEERCRED` identity before attaching an agent-only connection
marker. The earlier pid-zero scheme is invalid because Linux rejects it with
`ESRCH`.

Root identity authenticates the agent RPC; `CAP_SYS_ADMIN` authorizes the LVM
and local-EBS storage operations themselves. The agent receives no
`CAP_SYS_MODULE`, so the device-mapper snapshot target must already be loaded.
RPC payloads remain restricted to an operation, validated database UUID
namespace, and server-generated lease name. VG, origin LV, filesystem, mount
roots, AWS attachment pool, and provider lookup come from fixed local
configuration rather than caller input.

```text
 ordinary client                         privileged snapshot agent
 HTTP/2 immediately                      root identity + CAP_SYS_ADMIN
       |                                 SCM_CREDENTIALS(real pid/uid/gid)
       |                                             |
       +------------------+   +----------------------+
                          v   v
                /run/yesno/control.sock
                          |
          compare SCM_CREDENTIALS with SO_PEERCRED
                    |             |
            ordinary authz    agent-only marker
                                  |
                +-----------------+-----------------+
                |                                   |
          ClaimSnapshotAgentWork          CompleteSnapshotAgentWork
                |                                   |
                +---------- bounded broker ---------+
                                   |
                           server-owned lease state
```

Capture completion is a release-barrier RPC in the behavioral sense, not a
request for the agent to release anything. `yesnod` acquires and retains the
core backup lease, queues `CAPTURE`, and drops the lease only after the matching
successful completion. It then queues `MATERIALIZE` without the barrier.
Cleanup and reconciliation use the same authenticated queue. Operation IDs are
single-use, late completions after timeout are rejected, and a disconnected
claim eventually fails by the configured timeout rather than silently
releasing the barrier.

```text
 yesno-archive        yesnod / broker                 LVM agent
      |                     |                             |
      | BeginBaseSnapshot   |                             |
      +-------------------->| acquire backup barrier      |
      |                     | queue CAPTURE               |
      |                     |<--------- claim ------------+
      |                     |          lvcreate           |
 checkpoint -------------->| waits                       |
      |                     |<-------- complete ----------+
      |                     | drop backup barrier          |
 checkpoint <--------------| proceeds                     |
      |                     | queue MATERIALIZE            |
      |                     |<---- mount/recover/ro -------+
      |<--------------------| publish file-bearing lease   |
```

The deferred branch is an intentional inversion of execution, not ownership.
`yesnod` waits until the EBS snapshot is available and publishes a provisional
lease with no file list. `yesno-archive` keeps that lease alive and launches the
ECS task or EKS Job. The worker has a read-only restored source and writes only
the bounded database file set to shared staging. It never receives object-store
publication authority. The parent archiver waits for a successful exit,
validates and recovers the staged database, publishes it under its writer
lease, removes staging, and only then releases the server lease. `yesnod` has no
ECS or Kubernetes configuration or permission.

The archive is a second durability protocol above snapshot capture. It publishes
immutable WAL frames and base objects before the base manifest, advances remote
`state.pb` by compare-and-swap under a writer lease, persists its local cursor,
and only then acknowledges the leader's retention position. Replication batch
boundaries are not archive identity: history is normalized to individual frames
so reconnect can verify the retained chain.

`yesno-restore` selects a verified prefix by exact commit version, wall-clock
time, or durable tip. Wall-clock selection relies on I9 but resolves the cut
from commit-marker frames; descriptor times are only search hints. A restored
copy that will accept writes must raise its leadership term before publication.

Archive reclamation is disabled unless a retention window is configured and is
reachability-based rather than object-age-based. Each pass renews the writer
lease, re-reads `state.pb`, deletes references before their targets, preserves
unrecognized objects and bases with unknown times, and computes a WAL floor for
`( term, shard )` only when every retained base of that term names the shard.

`yesnoctl basebackup` is intentionally only a file-bearing-lease consumer. It
does not launch a deferred materializer and rejects a provisional descriptor;
deployments using deferred EBS materialization take continuous bases through
`yesno-archive`, or use local EBS materialization for an ad hoc base backup.

The Kubernetes operator resolves `spec.snapshot.backend: ebs` per instance from
the PersistentVolume bound to that instance's claim. The bound CSI handle is an
independent witness of volume identity; the API never accepts a caller-supplied
volume ID or derives one from the daemon mount it is meant to verify.
`WaitForFirstConsumer` makes this a two-stage rollout: the instance starts with
portable snapshots, reconciliation discovers the volume and renders EBS
configuration, and `Recreate` installs Pods carrying the new configuration
identity. `SnapshotBackendReady` reports that backup state without changing the
serving `Ready` condition, and it is not complete evidence until the current
Pods carry the rendered identity.

The real-AWS acceptance boundary is Terraform-owned rather than embedded in the
daemon. `e2e/aws/` provisions a run-tagged EC2/EBS environment and invokes the
ordinary scenario runner over SSM. `yesno-e2e/src/aws.rs` owns only processes,
RPCs, and observations inside that runner; production snapshot calls still go
through `snapshot/ebs.rs`.

The same Terraform stack has four EBS acceptance arms: local materialization,
deferred ECS, deferred EKS, and the Kubernetes operator on EKS. `ebs.py` has
the daemon restore its own snapshot while the privileged agent attaches and
mounts the clone. `deferred_ecs.py` and `deferred_eks.py` consume provisional
leases through unprivileged archive-owned workers; ECS uses a managed volume,
while Kubernetes uses a retained `VolumeSnapshotContent`, EBS-CSI claim, and
Job. The operator arm independently resolves bound-volume identities and rolls
the generated configuration. The local and deferred scenarios use different
data directories on the same source volume so later assertions cannot observe
earlier writes.

Each arm has passed against real AWS, but only in partial combinations. Report
`YESNO_AWS_ONLY` runs as partial evidence until one invocation proves that all
four arms, their shared infrastructure, and cleanup compose.

The deferred arms' claim is an absence, so the scenarios state it as one: no
`unix_socket`, no `mount_dir`, no attachment-name pool, no agent process, and a
container with no `--privileged` and no propagation flags. Do not grant a
deferred container a mount to make a failure go away — its shape is the
assertion.

Nothing in the stack is a Kubernetes provider, and there is no kubectl on
the runner. The in-cluster objects the archiver needs are created by a script
`gate.py` ships, POSTing JSON to the API server with curl under a token from
`aws eks get-token` — because a Kubernetes Terraform provider needs a working
cluster client at *plan* time for objects whose CRDs the same apply is still
installing. The archiver then authenticates as a namespace-scoped
ServiceAccount whose RBAC is exactly the four kinds it manipulates.

**Terraform owns the resources; a scenario owns the sequence.** Since
2026-09-02 the imperative half — image push, Systems Manager round trips, and
the runner script itself — is `e2e/aws/gate.py` running on the host through the
ordinary Monty runner, over the `cloud_*` intrinsics in
`yesno-e2e/src/cloud.rs`. Two shell scripts and a cloud-init `.tftpl` were
deleted with it. The division is the same one `op_*` keeps: the verbs are one
Terraform subcommand, one output, one image push, one wait, one remote command,
and they do not know the order. Do not fold the sequence back into a verb —
`terraform apply` reporting a single boolean for the whole gate is precisely
what this replaced. Additional `resource_tags` are copied to both the EBS
snapshot and restored volume, while the provider reserves its database and
lease keys. This gives the failure cleanup helper and IAM policy one exact run
boundary without weakening the database-UUID reconciliation boundary.

```text
 host scenario               runner scenarios            yesnod / EBS provider
 gate.py over cloud_*   ---> ebs.py over aws_*       ---> normal control RPCs
 apply, push, SSM run        tagged count oracle          local: agent mounts
        |                    ( privileged agent )         AWS SDK lifecycle
        |                           |                            |
        |               ---> deferred_ecs.py         ---> provisional lease
        |                    no agent, no mount       archiver -> RunTask
        |                           |                 managed EBS -> EFS
        |                           |                            |
        |               ---> deferred_eks.py         ---> provisional lease
        |                    no agent, no mount       archiver -> Job
 Terraform graph                    |                 snapshot content,
 VPC + IAM + EC2 + EBS              |                 CSI claim -> EFS
 + ECR + ECS + EKS + EFS            |                            |
        |                           |                            |
        +----------- run tag -------+----------------------------+
                  failure cleanup, then terraform destroy
```

The end-to-end resource lifetime is also a cleanup state machine; the active
lease table owns cleanup after publication. No externally usable lease is
published until capture and provider preparation succeed:

```text
  requested --> capturing --> preparing --> active lease
                  (barrier)    (no barrier)       |  ^
                      |             |             |  |
              failure |     failure |             +--+ keepalive
                      |             |             |
                      v             v             | release / expiry / shutdown
                    cleaning <------+-------------+
                      |  ^
              success |  | failure: retry
                      |  |
                      v  |
                     gone
```

Release is an authorization to clean up, not proof that cleanup already
finished. Cleanup is idempotent; a failed deletion remains registered and is
retried. Process restart reconciles provider objects in the exact
database-UUID namespace before admitting a new dependent lease. A lost
archiver is bounded by lease expiry, while a lost server is bounded by startup
reconciliation. This is why resource tags and names are part of correctness,
not merely cost attribution.

## Replication — `repl.rs`

The leader ships **raw on-disk WAL frames**, so the follower's apply path is the same decoder as crash recovery: one framing, one scanner, one fuzz target. A second wire format would be a second decoder that can drift, and drift here means a replica that silently disagrees with its leader.

Determinism is required at the level of set **contents**, not container **encoding**. Records are logical and carry no extent addresses; the follower runs its own allocator and checkpointer, so it may legitimately hold a Bitmap where the leader holds an Array for the same chunk. `fsck --compare` therefore means set equality, not byte equality.

**Both halves ship in `yesno-server::replication`.** `LeaderService` serves `Status` / `Subscribe` / `Ack` / `FetchBaseSnapshot`; `FollowerClient` bootstraps physically, streams frames into its own `shard-NNNN.wal`, and opens a `Db` — so the apply path *is* crash recovery, with no second decoder. `yesno_core::repl::Follower` supplies the bookkeeping the bytes cannot: it runs the leader's own consecutive-prefix watermark rule, so a multi-shard commit becomes visible only once every participant's records have arrived, and that is what an `Ack` reports. Keeping these types in the daemon crate aligns them with the authenticated listener, role transitions, retention floor, and server-owned snapshot leases they participate in.

**The follower refuses a leader that is not its own, before it writes a byte.** `Status` has reported `db_uuid` since M7 "so the follower must be able to tell leaders apart" and nothing compared it; the mistake surfaced only at `Db::open`, by which point a foreign multi-megabyte image was on disk. `bootstrap_shard` and `catch_up_shard` now check first, every call — not a `verify_leader` an operator must remember, which is the same forgettable shape as an identity written everywhere and read nowhere. The expected identity is re-read from the follower's own MANIFEST per check rather than captured at construction, because the operational order is *create the directory, seed the MANIFEST, then bootstrap*. A directory with no MANIFEST yet **adopts** the first leader it reaches and is bound to it thereafter — that is the case that made folding the check in look impossible, and adopting resolves it without leaving the check optional.

Base-image bootstrap preserves sparse files without reading or transmitting the
whole apparent 1 GiB segment. The leader scans bounded chunks and omits
all-zero runs; the follower sets the declared image length and writes only the
ordered runs, recreating holes. Because offsets are no longer contiguous, the
wire contract also validates chunk ordering and bounds plus the leader's exact
sent-byte count. Each shard is written beside its final path, its matching log
is cleared, and one rename publishes the complete image; final-path existence
is never evidence of transfer completeness.

**A follower that acks holds generations back.** `RetentionFloor` tracks each
authenticated `( follower, shard )` acknowledgement through a bounded
`RETENTION_GRACE` of silent windows; anonymous deployments conservatively
collapse reports to one identity because they cannot distinguish followers.
`Db::checkpoint` retains the minimum protected history and deletes only whole
sealed generations below it. `CheckpointPolicy::max_wal_bytes` ( 4 GiB by
default ) remains the hard ingestion-safety bound: crossing it may reclaim
through a lagging follower's floor, after which that follower must bootstrap
again.

Until 2026-08-25 the crate shipped only the leader, `Follower` had no caller anywhere, and the M7 gate hand-rolled a follower inside the test — so it proved that *the test's* follower could catch up. `Follower` also retained every decoded record in a buffer nothing drained: both a second apply path this section forbids, and unbounded growth in a long-running follower. Deleted.

## Tantivy edge — `yesno-tantivy`

**All fallible work ends before Tantivy starts searching.** Tantivy's
`Query -> Weight -> Scorer` path is synchronous, and its `DocSet` cursor
cannot return an I/O error. Embedded expression evaluation, Flight planning and
fetching, response validation, and stable-ID resolution therefore produce one
fully prepared query. No scorer performs storage or network I/O, and a partial
remote stream is never visible to a search.

**A yesno ordinal is a stable application ID, not a Tantivy `DocId`.** Tantivy
document IDs are segment-local and change after merges. The built-in resolver
scans one unique, single-valued `u64` fast field across live documents and maps
each stable ordinal to `( SegmentId, DocId )`. Missing and duplicate IDs are
errors by default. Ignoring missing IDs is an explicit eventual-consistency
policy, never a fallback.

**Prepared queries are generation-bound.** The binding includes segment IDs and
delete opstamps, so commits, deletions, and merges invalidate a query even when
a segment name happens to survive. Execution against another generation returns
an error instead of an empty result; an empty set is reserved for a valid query
that genuinely matched nothing. The query is a zero-score constant filter by
default and can be assigned a finite constant score explicitly.

**Remote consistency is named in the request.** The legacy expression
descriptor means the server's current snapshot. The `YSNQ` request envelope
can name an exact database version; the server calls `snapshot_at` and refuses
a reclaimed, future, or foreign version rather than falling forward. The
adapter preflights the exact Flight row count against a materialization ceiling,
then verifies non-null `u64` ordinals, strict ordering, and promised
cardinality before constructing the Tantivy query. A current query may restart
from planning after recoverable stale-ticket errors. A pinned query never
changes its requested version.


## PostgreSQL edge — `yesno-pg`

Three integrations in one crate — a foreign data wrapper, an index access method, and a table access method — because they share the option vocabulary, the transport layer and the ordinal↔`bigint` mapping, and a divergence in any of those three is a wrong-answer bug rather than a duplication.

**PostgreSQL owns callback memory according to the kind of access method, and the two contracts differ.** `GetIndexAmRoutineByAmId` copies an index routine into `CacheMemoryContext`, so an allocated `IndexAmRoutine` is safe. `RelationInitTableAccessMethod` stores the returned table routine directly in `rd_tableam`, so the `TableAmRoutine` must have backend lifetime; `tam::handler` constructs it once in `TopMemoryContext` behind an `AtomicPtr`. Any Rust wrapper cast through a PostgreSQL prefix struct is `#[repr(C)]`, because Rust otherwise does not promise the prefix remains first. These are ABI invariants, not allocation choices.

**A yesno table still needs an empty PostgreSQL storage fork.** The planner asks smgr for the relation's block count before any table-AM callback runs, so `relation_set_new_filelocator` calls `RelationCreateStorage` even though tuples live in yesno. That callback also implements transactional `TRUNCATE`: PostgreSQL gives the relation a new relfilenode there rather than calling `relation_nontransactional_truncate`, while yesno storage is keyed by the unchanged relation OID and therefore must be cleared explicitly.

**Planner identity and execution identity must travel as values, not borrowed pointers.** Join and upper `ForeignScan`s have `scanrelid == 0`, hence no `ss_currentRelation`; the relation OID is carried in `fdw_private` as a decimal string because `Oid` is `u32` while `makeInteger` accepts `c_int`. `GetFdwRoutineForRelation` likewise returns a per-relation copy, so foreign-server compatibility is tested with `serverid`, never routine-pointer equality.

**The bindgen boundary has a small, deliberate handwritten shim.** PostgreSQL's `static inline` helpers are not emitted by bindgen, so tuple-slot, item-pointer and index-build operations that need them reproduce those definitions locally. `BlockIdData` stores the block number in two `uint16` halves, high half first; treating it as a native `u32` byte-swaps the TID.

**The alignment that makes the index AM more than an adapter.** A PostgreSQL `ItemPointerData` is a 32-bit block and a 16-bit offset — 48 bits. A yesno ordinal splits into a 48-bit `Prefix48` and a 16-bit slot. Set `ordinal = ( block << 16 ) | offset` and the two coincide exactly, so **one container is one heap block** and `amgetbitmap` is one `tbm_add_tuples` per chunk over offsets the container already holds. `Container::len()` is O(1) on every representation, so "how many tuples on this page match" is free, which is why `amcostestimate` reports an **exact** row count where every built-in AM estimates from `pg_statistic`.

**The counterweight, recorded so it is not rediscovered as a disappointment.** A heap page holds at most ~291 tuples at the default `BLCKSZ`, so a container never approaches `ARRAY_MAX` and the bitmap representation is never selected. Every container is an array: two bytes per posting. Competitive with GIN's posting lists, but it is a sorted-array index and the Roaring compression thesis does not apply.

**Two TID packings, deliberately different, in sibling modules.** The index AM only *reproduces* TIDs PostgreSQL assigned, so it uses the full 16-bit offset field. The table AM must *invent* them, and an invented offset must be at most `MaxOffsetNumber` ( 2042 at 8 KiB ), not 65535 — so it packs `block * 1024 + ( offset - 1 )` and the value domain caps near 2^42. Confusing the two yields plausible TIDs pointing at wrong rows; each has a round-trip property test and they are asserted to differ.

**`u64` ordinals are exposed as `bigint` by bit reinterpretation**, which is a bijection — so equality, `IN`, membership, joins and `count(*)` are exact. Ordering is not: `int8` sorts `[2^63, 2^64)` *below* `[0, 2^63)`. No `pathkeys` on any foreign path, and a range straddling zero in `int8` lowers to **two** disjoint `u64` ranges. A one-range lowering silently returns nothing. The two access methods are immune ( TIDs are positive and bounded ), which is exactly why the hazard is easy to forget while working in them.

**`recheck` also makes dangling TIDs harmless, which is not obvious and changes how VACUUM is tested.** A key skipped by `ambulkdelete` leaves the index returning TIDs for tuples that no longer exist — but the heap scan re-evaluates the qual against the real tuple, so a stale TID is discarded whether it points at a dead tuple or at a live one that a later insert put in the freed slot. The cost is unbounded index growth, **not** wrong rows. The consequence: no row count can detect a skipped key, so the assertion that guards `ambulkdelete` is the Bitmap **Index** Scan's row count under `EXPLAIN ( ANALYZE )` — the index's output before recheck. Established by sabotage, 2026-08-30.

**The pushdown safety rule is `yesno-datafusion`'s, arriving at a second call site.** An unhandled conjunct may be dropped — it stays a local qual and the scan returns a superset. An unhandled **disjunct may never be dropped**: that shrinks the result and no filter above the scan can re-add rows that were never emitted. Its index-AM twin points the other way: the key is a hash, so a posting list is a superset and `recheck` must always be true. Both are wrong-answer failures with no error, so both are covered by a brute-force oracle rather than by review.

**A PostgreSQL transaction is not a yesno transaction.** Writes buffer per transaction and flush at `XACT_EVENT_PRE_COMMIT`, so `ROLLBACK` is correct and the common case is atomic — but yesno commits before PostgreSQL writes its commit record. A crash in that window leaves the two systems disagreeing; `fdw-two-phase-commit` tracks the durable prepare/resolve protocol needed to close it. Independently, a yesno table and a heap table in one query can **tear** because yesno does not store PostgreSQL xids; `tam-mvcc` records that cross-engine clock tradeoff. Isolation *within* yesno is a separate question and is honoured with Flight tickets, which record the version at mint time: `REPEATABLE READ` and `SERIALIZABLE` cache one per transaction and target, while `READ COMMITTED` caches one per executor statement and target. The executor hooks preserve one map across nested executor calls, clear it after the outer `ExecutorEnd`, and chain any hook installed earlier; the transaction callback is the error-path backstop because PostgreSQL `ERROR` can bypass `ExecutorEnd`. Thus repeated scans in one statement agree, but the next `READ COMMITTED` statement can see a newer commit. Do not conflate these boundaries: pinning closes the within-yesno one, while crash atomicity needs two-phase resolution and agreement with PostgreSQL's clock needs stored xids.

**Buffered writes are part of every read view until pre-commit.** A transaction that cannot see its own pending changes is incorrect even though commit and rollback later work. The overlay is therefore shared by all three read paths: ordinary FDW scans, the FDW `count(*)` branch, and table-AM execution. A buffer change must audit all three callers.

**Built by Bazel, not cargo** — a scoped exception to the plain-`cargo` rule,
because a `cdylib` dlopened into PostgreSQL must match one server's ABI exactly
and `cargo pgrx init` reaches that through unpinned machine state. The public
gate makes Docker the host boundary: its image build compiles both supported
server majors, the extension, unit runner and regression runner, and commits
the complete Bazel tree to the single all-in-one `yesno-e2e:local` image. MySQL,
OpenSearch, Elasticsearch, operator and filesystem artifacts and caches coexist
in that final stage; each gate still runs only its own fresh fixtures. See QUALITY_GATE § 1 for which
gate a change needs.

**Bazel owns immutable ABI inputs; the ordinary `yesno-e2e` runner owns the hermetic test.** `//e2e/postgresql:regress` supplies the PostgreSQL prefix, extension files, scenario, SQL, expected output and isolation specs as declared runfiles. The Python scenario composes the common `fx_*` verbs to copy the selected prefix, install the extension, initialize and stop the cluster, run the shipped Flight service on an ephemeral loopback port, execute byte-exact SQL fixtures, coordinate persistent isolation sessions with flushed marker comments rather than sleeps or `stdbuf`, diagnose backend death, and clean up. No specialized Rust entrypoint, shell launcher, helper server binary, host `diff`, or fixture-directory discovery remains on the test path.

**The PostgreSQL major is one build configuration, not machine state.** `--//:pg_version` selects a pgrx crate-universe hub, the extension's own `pgN` feature, and a sha256-pinned server archive together; every select has no default arm, so partial wiring fails analysis. PostgreSQL 17 and 18 need separate hubs because repository-rule feature resolution happens before analysis, but both consume the same authoritative `yesno-pg/Cargo.lock` — features are not lockfile data. The second resolver manifest is mechanically checked to have identical package metadata after normalizing its major and relative source path. Major-specific C-API shapes live in `pg_compat` ( planner cost signatures and tuple-descriptor layout ) or immediately beside the affected table-AM callback; they do not leak into planner semantics. The PostgreSQL gate runs the full SQL and isolation corpus against both majors.

## C ABI and MySQL edge — `yesno-c`, `yesno-mysql`

**The C boundary is a product boundary, not a MySQL bridge.** `yesno-c` has no
host concepts and no process-global database. `yesno_db_open` returns an opaque
owned handle, so one process can open independent databases, and every operation
takes that handle explicitly. `yesno_cursor_open` materializes one key from one
snapshot into an owned ordered vector. This spends memory proportional to the
set cardinality, but gives foreign callers a small cursor state machine with a
stable view and no Rust lifetimes crossing the ABI.

**The unsafe surface is pointer plumbing only.** The public header owns one
common contract: handles come from the matching open call, close consumes them
once, close cannot race an operation, and output buffers have their declared
size. Each pointer dereference and ownership transfer has a local `SAFETY`
comment. Every exported Rust function catches unwinding so a Rust panic cannot
cross the C ABI. Boolean results use `uint8_t`, errors use caller-owned optional
NUL-terminated buffers, and a strict C11 runtime smoke test exercises the actual
static library rather than a Rust-only imitation.

**The MySQL table is exactly one ordinal set.** The only accepted schema is one
`BIGINT UNSIGNED NOT NULL PRIMARY KEY`, and a `CONNECTION` value of `key=<u64>`
supplies the yesnodb key. The handler talks only to a C++ backend interface.
The embedded implementation owns `yesno_cursor` snapshots; the remote
implementation materializes one versioned Flight result into the same cursor
state machine. Point lookups and mutations stay atomic in either backend;
`COUNT(*)` is the exact set cardinality. Rename keeps the connection key. Truncate and drop clear it, so two
SQL tables naming the same key intentionally alias and either drop clears both
views.

**Transaction boundaries do not pretend to align.** YESNO advertises
`HA_NO_TRANSACTIONS`; insert and delete commit to yesnodb immediately, and MySQL
rollback cannot reverse them. A scan is nevertheless internally stable because
its materialized cursor comes from one yesno snapshot. The plugin owns one
backend, checkpoints or remotely compacts it during clean unload, and defaults
embedded files to a `yesno` directory under the MySQL data directory. Startup
selects `embedded` or `flight`; remote initialization probes the configured
endpoint before accepting the plugin.
The handler ABI is not portable. Bazel therefore overlays the handler onto a
sha256-pinned MySQL 8.4.0 source archive, builds the independent `yesno-c`
static library with PIC and the native client against sha256-pinned Arrow C++,
and links the loadable module and server in one graph. Arrow's two shared
libraries are bundled under MySQL's private library directory and resolved by
the plugin's relative runpath. The public gate makes Docker the host boundary:
its image build commits MySQL, Arrow, both native clients, the plugin and test
runners with the full Bazel tree to the same all-in-one `yesno-e2e:local` image
used by every other containerized gate, so later sessions reuse those artifacts
before starting fresh E2E servers.
The MySQL gate first runs the native client's hermetic protocol-validation unit
tests, then invokes `e2e/mysql/mysql.py` through the same runner and generic
`fx_*` fixture host as PostgreSQL. The scenario initializes two private copies
of that exact server. Embedded mode exercises the C ABI and byte-exact
mysqltest corpus; Flight mode loads the native C++ client against the shared
in-process Flight fixture. Both modes assert schema rejection, the unsigned
ordinal boundaries, forward and reverse range seeks, exact cardinality,
duplicate/NULL/update refusal, key aliasing and clearing, nontransactional
rollback semantics, and plugin lifecycle. The source-tree CMake path remains a
developer fallback and may consume either the adjacent Cargo workspace or
Bazel's prebuilt library and header.

## Invariants

These hold at every observable boundary. `tests/proptest_oracle.rs::assert_invariants` checks the set-level ones; `codec::validate` checks the container-level ones; **`tests/invariants.rs` checks the storage ones ( I2-I7 ) directly**, and `store/fsck.rs` checks slot liveness, which is a different question. This paragraph used to attribute the storage invariants to `fsck` alone — it does not assert I2, I5, I6 or I7, and until `tests/invariants.rs` existed none of them was checked by anything.

**Storage level.** These are cited *by number* throughout the module `//!` comments, so the numbering is load-bearing — do not renumber.

- **I1 ( LE-only )** — a big-endian host is refused at open. There is no byteswap path.
- **I2 ( Extent immutability )** — a published extent is never mutated in place. Containers alias the mapping, so violating this is UB, not a torn read.
- **I3 ( Alloc-at-checkpoint )** — extent allocation happens *only* in the checkpointer, single-threaded. The write path never allocates file space. This is what makes the deferred free list need no persistence, and what makes WAL replay unable to diverge from allocator state.
- **I4 ( Checkpoint barrier )** — a checkpoint persists only state at or below the global visible watermark. The on-disk image is therefore always a globally consistent snapshot, which is what makes recovery redo-only and physical bootstrap of a follower correct by construction.
- **I5 ( Per-shard cv monotonicity )** — each shard's WAL strictly increases in commit version, guaranteed by assigning the version while holding all participating shard locks. This is what makes recovery's discarded records a clean suffix.
- **I6 ( Append-only mappings )** — segments are created, never recreated, never unmapped while a `Buffer` may point into them, and the file never shrinks. Truncating under a live mapping raises `SIGBUS`, which is not catchable as a `Result`.
- **I7 ( Key locality )** — all chunks of one key live in one shard. Shard by `key`, never by `prefix48`.
- **I8 ( Ordinal ceiling )** — the ordinal universe is `[0, ORDINAL_MAX]` where `ORDINAL_MAX = 2^64 - 2`. **`u64::MAX` is not an ordinal.** Reserving it is what makes every cardinality fit a `u64` ( a full set has `len() == u64::MAX` ), makes the complement representable, and lets a half-open range name the whole universe as `[0, u64::MAX)`. Enforced at the fallible boundaries — `Db` mutators, `WriteBatch::commit`, and the roaring import return `CodecError::OrdinalOutOfRange` — and since every path to durable state is one of those, an out-of-range ordinal cannot reach the WAL, a container, or a page. The infallible `OrdSet` mutators carry it as a documented precondition with a `debug_assert`. There is no *structural* on-disk check: a container is prefix-agnostic so `codec::validate` cannot see which chunk it belongs to, and `fsck` does not decode payloads. See `i8-has-no-structural-check` in `JOURNAL.md` ( closed ).
- **I9 ( Commit-time monotonicity )** — every commit marker carries the wall-clock time its commit version was assigned, that time is non-decreasing in commit version, and every participant of one multi-shard commit records the *same* value. Guaranteed by stamping inside `VersionOracle::begin`, under the same lock that assigns the version, clamped to `max( now, last + 1 )` so a backward system-clock step cannot invert two commits. The floor survives a restart whose WAL prefix has already been checkpointed away because the checkpoint persists the clock's high-water mark in the superblock. This is what lets a wall-clock recovery target name a prefix at all: without it there is no version whose prefix is exactly "everything at or before T". Records written before stamping existed carry no time, and that absence is reported as unknown rather than as the epoch — a wall-clock restore over an unstamped prefix refuses rather than rounding. Asserted in `tests/invariants.rs` and under contention in `tests/concurrency.rs`.

**Set level**

- `prefixes` is strictly ascending, and `prefixes.len() == containers.len()`.
- Every `Prefix48 < 1 << 48`.
- No container in an `OrdSet` is empty.
- `len` equals the sum of container cardinalities.

**Container level**

- Array: sorted, unique, `1 <= card <= ARRAY_MAX`.
- Bitmap: exactly `BITMAP_WORDS` words, cached `len` equal to the true popcount.
- Run: intervals sorted, non-overlapping, and non-adjacent ( adjacency must be merged, not stored as two runs ).
- `rank` and `select` are mutually inverse over the container's contents.

**Matrix level** — `matrix/`, over the canonical `BitMatrix` form.

- **Padding tail is zero.** A row occupies `ceil( cols / 64 )` words and every bit at or above `cols` in its last word is unset. This is an invariant and not a convention: `BitMatrix` derives `PartialEq`, which compares words, so a dirty tail makes two equal matrices compare unequal *and* makes `count_ones` overcount. Every operation that writes a whole word must mask it.
- **Row-major, always.** The canonical form has one layout; `Layout::order` describes the *source*, not the value. A `ColMajor` source is normalized by transposing during the read, so no kernel ever sees a column-major matrix.
- **The seam is at the boundary.** Arbitrary strides and chunk straddling are handled in `pack/gather.rs`, `pack/seek.rs` and `matrix/sink.rs` and nowhere else. A kernel that shifts or carries across a word for layout reasons is in the wrong file.
- **Addressed ordinals obey I8.** `Layout::ordinal_at` returns `None` rather than wrapping, so a matrix reaching `u64::MAX` is unaddressable rather than silently truncated.

**Bignum level** — `bignum/`, over the canonical `BigUint` limb form.

- **No trailing zero limb.** Little-endian `u64` limbs with `limbs.last() != Some( &0 )`, so zero is the empty vector and `limbs.len()` is exactly `bit_len().div_ceil( 64 )`. An invariant and not tidiness, for two reasons: `BigUint` derives `PartialEq`, which compares limbs, so `[5]` and `[5, 0]` would be unequal; and `Ord` compares lengths first, which is only sound because of it. `BigUint::is_normalized` is the debug-time guard, and subtraction is the one operation that must re-establish it rather than merely assert it.
- **Ordering is hand-written, never derived.** A derived `PartialOrd` on a little-endian limb vector compares `limbs[0]` first — the *least* significant limb. It is worse than an ordinary wrong answer because it is right whenever the operands are equal-length and differ in the top limb, which is what a uniform generator produces almost always. The regression test is a same-length pair differing only in a low limb.
- **The seam is at the boundary.** Arbitrary `stride`, an arbitrary base offset and chunk straddling are handled in `pack/gather.rs`, `pack/seek.rs` and `bignum/sink.rs` and nowhere else.
- **Addressed ordinals obey I8.** `IntLayout::ordinal_at` and `base_of` return `None` rather than wrapping, and `IntSink::place` reports `OrdinalOutOfRange` with a `u128`-computed, `u64`-saturating ordinal, so an integer reaching `u64::MAX` is unstorable rather than silently truncated.
- **Width lives in the layout and nowhere else.** A `BigUint` carries no width, because `a + b` may need one limb more than either operand and a fixed-width value would make every kernel a modular kernel.
- **Overflow is refused, never clamped, and never silently wrapped.** Arithmetic cannot overflow — the limb vector grows — so overflow exists only where a value meets `width_bits`, at `IntSink::place`, which returns `CodecError::Invariant`. A caller wanting the cyclic reading writes `v.truncate( width_bits )`, spelled at the call site so the width appears once. There is no saturating form. The reason is **not** that saturation composes badly: saturating addition is associative and `sat` nests exactly as truncation does, both checked rather than assumed. The reason is that **the reader cannot saturate** — a narrow read gathers `W` ordinals and cannot see the bits above them, so it *is* `x mod 2^W` by construction, and truncation is the only write rule that agrees with it.

**Stream level**

- Strictly ascending prefixes; never an empty container.
- `peek_prefix` is a lower bound on the next yielded prefix.
- `cardinality_dyn` equals the cardinality of the materialized result.

## Testing Architecture

Each test file exists to catch a failure class the others structurally cannot. See `.agents/docs/QUALITY_GATE.md` §3 for which one a given change must satisfy.

- `proptest_oracle.rs` — properties against `BTreeSet<u64>`. Generators are boundary-biased on purpose ( clustered, runny, mixed, and **ceiling** ordinals ); uniform random `u64`s would put one ordinal per chunk, so arrays would never fill, bitmaps would never appear, and run containers would never be produced. Biased on *three* axes, and the third was missing for four milestones: cardinality and prefix pattern were covered exactly as the comments promised, while the largest ordinal any generator could emit was nowhere near `u64::MAX` — which is how an overflow in the multi-chunk range walk survived. `ceiling_ordinals` and `ceiling_range` close it; the latter bounds a range's *width* without bounding its *position*, which is why no u64-level range property existed before ( a uniform `(lo, hi)` is almost always a span no oracle can enumerate ).
- `differential.rs` — the M0 gate: semantic agreement with `RoaringBitmap` / `RoaringTreemap`, plus byte-level identity in both directions ( we parse what `roaring` writes, `roaring` parses what we write ).
- `expr_equivalence.rs` — the M1 gate: random expression shapes evaluated lazily, eagerly, and against a `BTreeSet` oracle; plus `cardinality == collect_set().len()`.
- `allocation.rs` — a thread-local counting allocator asserts allocation budgets. Counters are thread-local rather than global on purpose, so the tests do not silently require `--test-threads=1`. This is the only layer that can see a *parallel implementation* being chosen wrongly, because both paths return the same answer: it is what caught `Snapshot::cardinality` materializing every container instead of reading `card_m1`, and `Expr::open` folding an n-way OR pairwise instead of using the shared accumulator. Never raise a budget to accommodate a change — the budgets encode "does not scale with chunk count", not a measurement.
- `durability.rs` — every read method against data that is genuinely on disk, oracle-checked. **Always reopens before reading**, because `checkpoint()` does not clear the memtable, so a read on the same `Db` instance is answered from memory and never reaches the store. That blind spot hid three separate bugs; this layer exists so it cannot hide a fourth.
- `zero_copy_mvcc.rs` — a live reader holding a container that aliases the mmap, across checkpoints, reclamation, reopen, and the death of the `Db` itself. No other layer can reach this: `crash_matrix` restarts the process, `allocation` counts heap bytes these pages never occupy, and the `store::alloc` unit tests drive one allocator by hand. Every set here is deliberately larger than the 3-ordinal inline limit, and every assertion is on **contents** — cardinality comes from the index and stays correct even when the payload is gone.
- `crash_matrix.rs` — the M3 gate: WAL truncation and corruption at **every byte offset**, superblock tears at every byte, and every multi-shard participation combination. Deterministic, not randomized — the watermark advance and the three-condition reclamation are where the real bugs are, and they need exhaustive rather than sampled coverage.
- `concurrency.rs` — the only multi-writer coverage in the tree, and the only place several properties are *observable at all*: group commit cannot batch with one writer, and the WAL generation's rollover guard ( "the physical end still equals the checkpoint snapshot" ) is always true single-threaded. Stress, not a model checker — a pass means the interleavings that occurred were correct. Skipped by `scripts/valgrind.sh`, which serialises threads onto one core and so removes the interleavings being measured; AddressSanitizer is what covers it instead.
- `invariants.rs` — I2 through I7 asserted directly rather than assumed. Every one was named in a module `//!` block and none was mentioned by any test until 2026-08-25. I2 in particular is checked by *reading the file back*: a pinned snapshot, a bounded walk of `allocated_bytes()` slab by slab, excluding bytes that were previously zero, so it fails on an in-place rewrite of published space rather than on ordinary growth.
- `e2e/scenarios/*.py` — operational sequences ( open, ingest, checkpoint, close, reopen, query ) scripted in Python and run by `monty` against a real `Db`. Adding one is adding a file. Python's own `set` is the oracle, so the expectation is written as `sa & (sb | sc)` rather than recomputed in assertions. The runner refuses a scenario with no `assert` and one that never calls a verb. **See `.agents/docs/TESTING.md`** for the harness in depth — the verb surface, the handle model, the admission test for what belongs in `scenarios/`, and why a timing loop must run in the host.

  Since 2026-08-26 this is also where the **measurement fixtures** live. They were `yesno-core/examples/*.rs`, and they moved because `cargo clippy --all-targets` type-checks an example while **no gate executes one** — so a fixture whose subject silently changed kept compiling and kept reporting a number nobody re-derived. That is not hypothetical: `Xor(disjoint) -> Or` stopped losing money when `concat_disjoint_or` landed and nothing noticed until the fixture was rebuilt by hand months later. As scenarios they run in `cargo test -p yesno-e2e`. Only `examples/readme.rs` stayed behind, because its whole value is that it is *Rust that compiles*. Three verb families were added for the move:

  - `sb_*` / `set_*` / `ct_*` / `ops_*` ( `yesno-e2e/src/eager.rs` ) — build an `OrdSet` without a database, walk it chunk by chunk, run the container kernels by hand. The builder takes **arithmetic progressions** rather than values ( `sb_stride( sb, base, step, count )` is one host call whatever `count` is ), because every fixture operand has that shape and materializing 8.6 M ordinals as a Python list does not work. There is deliberately **no `set_range`**: yesno already spells ranges two ways and a third would be a trap.
  - `q_plan` / `q_repr` / `q_kind` / `q_arity` / `q_child` / `st_*` ( `yesno-e2e/src/lazy.rs` ) — the planner as an object of study, and the raw `ChunkStream` cursor. `st_open` lowers **without** planning, so `st_open( q_plan( e ) )` and `st_open( e )` are the two columns a planner measurement compares.
  - `yn_arg( name, default )`, from the runner's `--arg name=value` — what replaces an example's `--big` / `--sparse`. Absent under `cargo test`, so the gate runs a small corpus and the example's own is a deliberate act.

**What does not migrate, and must not be pretended away.** A repetition loop written in Python times monty: one host call is ~1 µs, against the tens of nanoseconds `rule_economics` measures. `q_time( expr, iters, terminal )` runs the loop in the host and covers every column whose subject is an `Expr` — those reproduce the Rust fixture to within noise. A column whose subject is a **hand-written walk expressed in Python** cannot be timed at all.

**And a fixture whose subject is a hand-written walk does not belong here at all.** `and_shape` and `aligned_eval` were migrated on 2026-08-26 and moved straight back out: their algorithms — a k-way `IntersectAll`, an aligned-grid evaluator — exist in Python and **nowhere in `src/`**, so their assertions were about the scenario file. They live in `.agents-workspace/tmp/prototypes/` as research fixtures, still runnable by path. The test for admission is blunt: *does a change to `yesno-core` fail this in the way the assertion is phrased?* A scenario that reimplements an operator and checks its own answer against yesno is testing the reimplementation.

Moving them silently orphaned **thirty-four verbs** — a third of the surface, with no caller left in the suite and nothing failing. `every_verb_has_a_caller_in_some_scenario` in `tests/scenarios.rs` is the guard, and `set_api.py` / `streams.py` are the coverage that replaced them.
- `e2e/postgresql/` - the hermetic PostgreSQL suite: the Monty scenario, SQL and isolation inputs, byte-exact expected outputs, and the Bazel target that invokes the ordinary runner. Bazel owns the sha256-pinned PostgreSQL server and extension artifacts; the scenario composes `yesno-e2e/src/fixture.rs` verbs for the private prefix, extension installation, Flight fixture, PostgreSQL lifecycle, persistent two-session schedule, crash diagnostics and cleanup. Every fixture is an explicit runfile, and the suite runs through `scripts/gate-pg.sh` for every supported PostgreSQL major while remaining outside the ordinary Cargo scenario walk.
- `e2e/aws/gate.py`, `e2e/aws/ebs.py`, `e2e/aws/deferred_ecs.py`, `e2e/aws/deferred_eks.py` and their two verb modules - the opt-in real-AWS EBS gate, as a host scenario and three runner scenarios. `gate.py` applies the Terraform stack, reads its outputs, pushes the slim runner image to a per-run ECR repository, waits for the Systems Manager channel, provisions the runner, ships all three runner scenarios to it and destroys; `ebs.py` is the local arm's behavioural oracle and asserts exact tagged-resource counts, the daemon/agent privilege split, and startup reconciliation after a crash with a live lease; `deferred_ecs.py` and `deferred_eks.py` are the deferred arms', and each asserts a failed worker and a natural one leave the same nothing behind - no provider snapshot, no materializer-restored volume, no staged directory - around a base the archiver could only have published through a worker that exited zero. Both also require a *nonzero* volume count while the worker's volume exists, because a tag filter that matched nothing would satisfy every zero-check in the file. The `cloud_*` verbs are intrinsics and hold no sequence; the runner scripts are text in `gate.py`, not a Terraform template, so changing what the runner does needs no recompile. Every `cloud_*` verb refuses without `YESNO_AWS_GATE=1`, because `every_advertised_verb_is_dispatched` calls every verb and this one bills.
- `e2e/operator/operator.py` and `yesno-e2e/src/operator.rs` - the opt-in live Kubernetes scenario and its `op_*` host verbs. The ordinary `yesno-e2e` runner remains the only endpoint: Python owns the lifecycle and assertions, while the narrow verbs own Docker, kind, kubectl, a unique cluster and kubeconfig, image tags, bounded waits, diagnostics, and cleanup. The scenario creates a leader and follower with independent retained PVCs, checkpoints acknowledged data, scales the leader to zero, requires fenced automatic promotion and follower rejoin, then verifies both old and new writes. `scripts/gate-operator.sh` selects that scenario explicitly because the routine Cargo suite must not require a Docker daemon or privileged local cluster.
- `benches/setops.rs` — baselines measured against the `roaring` crate as the absolute reference rather than against our own past numbers. A benchmark must assert its own shape: `binary_ops_dense` checks that its operands really are bitmaps, because an earlier version silently measured array containers and sent two specialization attempts in the wrong direction.
- `examples/readme.rs` — the README's opening snippet as a compiled target, so an API rename breaks the build instead of leaving the front page wrong. The **only** remaining example, and it must stay one: a scenario could check the same semantics and could not check that the code block on the front page still builds. Do not add a measurement fixture here — that is what `e2e/scenarios/` is for, and the reason is directly above.
- `e2e/scenarios/rule_economics.py` — whether the disjointness rewrites pay for themselves, measured as `( unplanned exec - planned exec ) - plan cost`. A **reconstruction**, not a recovery: the original three-agent audit ran as subagents whose transcripts were never recorded, so this was rebuilt from `rules-that-lose-money`'s description and says so at the top. It confirms the recorded figures for `And(disjoint)` to within a few ns — including the load-bearing one, that the unplanned leapfrog stays flat ( 50 -> 78 ns ) while operands grow 10 000x — and it is what caught `Xor(disjoint) -> Or` having silently stopped losing money once `concat_disjoint_or` landed. Every timing column is an `Expr` terminal, so `q_time` carries the whole table. It asserts what the Rust original only printed: that `And(disjoint)` plans to `Empty`, `AndNot(disjoint)` to its left operand and `Xor(disjoint)` to something no longer an `Xor`, checked with `q_kind` rather than by comparing `Debug` strings. The borrowing-walk timing column is the one thing lost in migration — it is a hand-written walk, so only its agreement is checkable.
- `e2e/scenarios/set_api.py` — the eager surface: `OrdSet` construction, the chunk layout underneath it, and the container kernels, all against Python's `set`. Covers what `expr_equivalence.rs` cannot reach from inside — that a stored chunk is never empty, that cardinality is the sum of the chunks, that `rank` and `select` invert, that `partition_point_in` matches a linear count over every sub-range, and that `ops_*` reports an empty result as `None` rather than as a zero-length container. It also pins `db_is_durable` in its **true** form ( a property of how the database was opened, constant across commits and checkpoints ) rather than as the durability check its name suggests; see `is-durable-reads-as-a-claim`.
- `e2e/scenarios/streams.py` — the `ChunkStream` contract and the planner, driven one call at a time from outside. `peek_prefix` reports without consuming and is a **lower bound, not a promise** ( XOR and ANDNOT may name a prefix whose chunk then cancels ); `seek` is monotone positioning and never rewinds; `next_cardinality` is a *parallel implementation* of `next_chunk` and only comparing the two can see it decay back into `next_chunk().len()`. Plus: planning preserves semantics on shapes that fire a rule and shapes that cannot, and is idempotent.
- `e2e/scenarios/aged_state.py` — space under sustained churn, reported as slab counts rather than file length ( `grow_to` rounds to 1 GiB segments, so file size is a step function ). `--arg keys= --arg per_key= --arg rounds=` scales the corpus; `--arg spread=100000` is one ordinal per chunk, the regime the cost model is built on and the only one where the index's share is visible: **0.18% dense, ~100% sparse**. Fresh-only numbers are misleading here, which is why the gate is on the aged state. It adds the check the Rust original never made — that the churn *preserved* what it rewrote, each key holding its last generation, and that `fsck` is clean afterwards, since a number produced by a database that lost data measures nothing. Its sparse assertion is on **which classes are allocated** ( only the index class, zero payload extents ) and not on the index's byte share, which is 31-46% here and is an artefact of slab occupancy rather than the structural claim.
- `e2e/scenarios/{range_ingest,wal_size,nary_or,ckpt_under_reader}.py` — the four small fixtures: ingest cost for a contiguous range against the same ordinals one at a time, WAL bytes per ordinal by write shape, k-way union against a pairwise fold, and checkpoint cost under a pinned reader. All four were "reported, not asserted" as examples, and each gained the assertion it was missing:

  - `range_ingest.py` — the two ingest paths must produce the *same set*, and a checkpointed contiguous range must be **all runs**. The memtable's encoding is not the stored one ( `run` + `bitmap` in memory, `run` + `run` after `optimize()` at checkpoint ), so both are pinned separately; checking only the first would have claimed something false.
  - `wal_size.py` — every shape is reopened **without a checkpoint**, so the numbers are only reported if the WAL actually replays them. Reproduces the design's predicted ordering: 0.0 B/ordinal contiguous ( the range record collapses it ), 2.0 scattered inside one chunk, 9.9 wide.
  - `nary_or.py` — `union_all` and the fold must agree on *contents*, not only cardinality, and the union must be order-independent, checked against a reversed fold.
  - `ckpt_under_reader.py` — the comparison the example left to the reader is the finding, so it is asserted: a held reader must leave the deferred list strictly larger, releasing it must drain it, and `fsck` must be consistent **while the pin is still held**.

- `scripts/valgrind.sh` — the UB gate. Covers `store/segment.rs`, which holds both of the crate's `unsafe` blocks. Proves memory safety, **not** reclamation logic: reuse of a slot inside a valid mapping is not a memcheck error.
- `scripts/miri.sh` — **withdrawn 2026-08-29 and gated by nothing.** It was the M2 UB gate for casts, in two tiers ( tier 1 `buffer:: container:: ops::` under `-Zmiri-symbolic-alignment-check`; tier 2 `index:: store::extent store::packed store::superblock store::checksum` without it, because `crc32c` rounds pointers itself and the symbolic check calls that a false positive ). It is kept on disk and runnable by hand, but `scripts/gate.sh --deep` and CI's `deep` job no longer invoke it. **The reason is cost**: MIRI interprets, so the price is set by the fixtures a test builds rather than the kernel under test, and `ops::run::tests` ( >900 s ) and `matrix::seek` ( 42 min 43 s on one test, killed ) both outgrew it. Note what the removal did *not* cost: those filters were always the whole coverage — `db::`, `wal::`, `store::alloc`, `store::fsck` and most of `stream::` were in neither tier, and `store/segment.rs` was refused outright. See `QUALITY_GATE.md` §7 for what replaces the cast checking ( `bytemuck`'s checked casts ) and what does not.
- **Sanitizers**, via `scripts/gate.sh --deep` — AddressSanitizer over the whole core suite, and ThreadSanitizer over `concurrency` with `-Zbuild-std`. TSan is the **only** race detector here: Valgrind serialises the threads away, and ASan does not look for races.
- `fuzz/` — not in CI ( needs nightly ), run per `QUALITY_GATE.md` §7. The defect shape it exists to find is **structurally valid, semantically impossible**: right length, in-range cardinality, container that cannot exist. Length and truncation errors were always caught.

### Three failure modes this project has actually hit

Worth stating, because all three keep recurring:

- **Documentation that describes the correct design while the adjacent code does something weaker.** Well past four occurrences now: `codec::decode`'s never-panic contract against a function that checked only lengths; `card_m1` making `len(key)` an index-only operation, in three documents, against a `Snapshot::cardinality` that decoded every container; a torn slab-metadata write "costing a rebuild, not data" against an `fsck` whose rebuild would have freed the index; I3's free-bitmap argument against a reopen that orphaned every pending extent. When a `//!` block explains behaviour, changing the behaviour means changing the block in the same commit — and **reading one is not evidence the code agrees with it**.

  A specific tell, seen twice in this document: when a correction is written *next to* the stale text instead of over it, both then read as authoritative and the file contradicts itself. Two adjacent paragraphs disagreeing is a stronger signal of drift than any single claim.

- **Tests that cannot fail.** A regression test is worth nothing until it has been observed failing against the unfixed code. A property whose generator cannot reach the boundary — starts drawn from `0..200` for a bug that needs 65535, or an ordinal generator that tops out below 2^25 for a bug at `u64::MAX` — reads as coverage and provides none. The same applies to a *leaf pool* nothing selects from, and to a test whose subject is a function production does not call.

- **Tools that cannot see their subject.** The newest, and the one that hides the other two. An unwired-machinery sweep matching bare identifiers reported a clean tree while `repl::Follower` was reachable from nothing — because `apply`, `visible` and `cursor` are words that appear everywhere, so **a type can be entirely unwired while every one of its methods looks used**. Rewritten to key on type names, it reported "74 types, zero unwired" — also clean, also blind, because a type's own `impl` blocks count as references to itself.

  Both were caught the same way, and it is the only check that works: **run the tool against a subject you already know is broken.** A tool that cannot find a known defect has told you nothing about the unknown ones. Do not report a clean sweep, gate or benchmark without that step.
