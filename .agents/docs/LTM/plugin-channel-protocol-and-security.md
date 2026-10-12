# Plugin Channel Protocol and Security

## Summary

`yesnod` serves plugin peers over a Unix socket -- reads throughout, and writes as one atomic frame since 2026-10-04 -- while retaining sole ownership of the database directory and snapshots. Each connection owns a session and, when available, a sealed `memfd` arena; socket closure lets the daemon revoke its snapshots even if the peer does not cooperate. The protocol, admission limits, and cutover order are part of the correctness boundary, not merely deployment settings.

## Key Facts

- A lane has 8,192 bytes, the exact maximum of array, bitmap, and run container encodings. Arena lane `i` begins at `arena_off + i * LANE_BYTES`; descriptors carry kind and count, from which payload length follows. The current wire contract represents a run as inclusive `[start, end]`, converted from the stored Roaring `( start, len_minus_1 )` form.
- The protocol codec lives in `yesno-plugin/src/ipc.rs`, not `yesno-wire`: there is one first-party protocol consumer and no cross-crate boundary for `yesno-wire` to bridge. Request, response, and notification kinds occupy separate ranges.
- A session owns snapshot handles. Closing a snapshot with live lane handles is refused. `SnapshotLoad` and `SnapshotKeyRange` page by value on a pinned snapshot, with `after` and raised `lo` continuations; `more` requires fetching one extra item.
- The default peer cap is eight connections and the default snapshot cap is 64 per session. Admission reserves an atomic slot before spawning or allocating, so concurrent connects cannot exceed the cap. An admitted peer's snapshots consume the database-wide reader registry.
- A socket file mode and kernel `SO_PEERCRED` are separate access checks. The default credential policy accepts the daemon uid and root; configured uids may widen it. Missing credentials and malformed mode settings fail closed.
- Before follower rebootstrap, the database slot is emptied, peers are notified and disconnected, and their sessions drop. Emptying the slot first prevents a new connection from acquiring an old snapshot during cutover. The listener stays up for reconnection to the new generation.

## Details

### Payload, arena, and fallback

The 8,192-byte lane bound follows from array `4096 * 2`, bitmap `1024 * 8`, and run `2032 * 4`. A descriptor exceeding its slot signals protocol disagreement and is refused. The arena reserves `max_lanes * LANE_BYTES` per handle without a free list; untouched `memfd` pages do not consume resident memory. `F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL` prevents a peer from truncating a mapped arena and causing `SIGBUS`. The descriptor is passed with `SCM_RIGHTS` and received with `MSG_CMSG_CLOEXEC`.

The inline fallback sends payloads in frames when an arena cannot be created, including Linux descriptor exhaustion or `ENOSPC`. The running server must actually select `Session::new_inline`; a unit-tested constructor alone did not establish portability. Inline capacity is derived from `lanes * blocks` fitting `MAX_INLINE_PAYLOAD`, so the server never advertises a legal width it cannot encode. A failed response encode yields a `Fault` frame instead of silently closing the connection. The arena path keeps a large batch independent of frame size: its block frame carries only five-byte descriptors.

`BlockAdvanceMany` batches blocks over one socket exchange. The socket remains necessary even with shared memory: its closure revokes the session, and its server-write then peer-read order publishes arena contents. An eventfd or futex would not replace either property. The three-arm performance study and its one-byte-per-lane limitation are in [Plugin Shape Performance](plugin-shape-performance.md).

### Snapshot reads and limits

`SnapshotCardinality`, `SnapshotContains`, `SnapshotMax`, `SnapshotLoad`, and `SnapshotKeyRange` answer from one pinned version. Load pages seek through a lazy `key_stream`; key-range pages initially re-enumerated the whole database, making repeated small pages quadratic. `Snapshot::key_range_limited` now collects at most the requested prefix per shard and merges those prefixes, requesting `want + 1` for the continuation flag. The per-shard bound is sound because each shard's index is ascending and keys are partitioned by shard. The memtable still scans in memory, bounded by its flush threshold.

`ipc::MAX_LANES = 4096` bounds decode structure; it is distinct from the server's advertised lane budget, whose default is 1024. A 265-lane query can use three 128-lane handles on **one** snapshot, preserving a single version. Using a second snapshot for overflow could straddle a checkpoint. The inline greeting instead advertises the smaller limit its frame capacity supports.

**The channel serves writes as of 2026-10-04**, as a single `Apply` frame carrying up to `MAX_WRITES` entries and answered by `Committed { version, changed }`. The three prerequisites recorded here previously -- a transaction identity, an unambiguous commit point, and a response for a disconnect after commit -- are answered by that shape rather than waived:

- **Identity** exists so a retry is safe when the first attempt may already have landed. Every write op is idempotent and a frame's entries are applied in arrival order, so replaying a frame leaves the state it would have left anyway. There is no non-idempotent retry for an identity to make safe.
- **The commit point** is the frame: one `Apply` is one `WriteBatch::commit`, and the version it produced is in the reply. Nothing is staged, so no commit point can be in doubt.
- **A disconnect after commit** needs no stored reply: the peer retries, which is safe by the first point, or reads the key back, since the same socket serves reads and `SnapshotOpened` carries a version. Concretely, a peer whose connection drops around an `Apply` can infer only that the batch either committed entirely or not at all -- never partially, because the batch is validated before anything is staged and committed as a unit. Reconnecting and re-sending is correct in both cases; reconnecting and reading is how it learns which happened, if it needs to know.

The advertised write cap is `max_writes` in the greeting, and it is the number to trust rather than the `MAX_WRITES` constant: a server may be configured lower, and `Apply` is enforced against what it advertised. This mattered once already -- the constant shipped at 16,384 while `Apply` was bounded like a descriptor frame at 65,536 bytes, so the real limit was 2,621 entries and a frame at the advertised figure failed in the peer's own encoder. The cap is now derived from the entry limit at compile time, and `Apply` is a third payload class so that raising any one of the three cannot widen the others.

What the shape does **not** offer is atomicity across frames -- a batch wider than one frame is several commits, as Flight's `PUT_INSERT` is per record batch. A caller needing one atomic bundle wider than a frame still wants Flight `PUT_APPLY`. That is the deliberate price of not needing the three answers above; a multi-frame transaction here would reintroduce every one of them.

Two refusals are part of the boundary. A **follower** refuses with `WRONG_ROLE`: a replica that applied a local write would diverge from its leader with nothing able to detect it, since replication ships the leader's log and the extra data is neither overwritten nor reported. An **empty slot** refuses with `UNAVAILABLE` rather than waiting, because blocking would hold a serving thread across an unbounded operation. Admission remains the uid check at connect time: a peer permitted to connect may write, on the same footing as its existing permission to read everything.

A replay changes state zero times but `changed` is not zero -- it counts operations that altered the set as they were applied, so a replayed `DeleteKey` followed by inserts reports work done while leaving the state untouched. Idempotence is a claim about the state a frame leaves, not the work it does getting there. For a purely additive batch, which is the common retry shape, `changed` is zero.

### Admission, access, and cutover

A connection reserves an atomic admission slot before its arena or thread is created and releases it on exit. Counting only registered peers allowed simultaneous connects to race past the cap. A session refuses new snapshots after its cap; silently evicting one would invalidate a handle the peer still holds. These limits also protect the process-wide 4,096-slot reader registry and the reclamation floor.

The socket bind refuses a non-socket path or a live listener and unlinks only a stale socket that refuses connections. The old unconditional unlink could redirect new peers to another database if two instances used the same path. Shutdown acts on the held listening descriptor instead of reconnecting through a pathname that might have changed. The parent directory remains the ultimate pathname boundary.

The server obtains peer uid through `SO_PEERCRED`; a failure is a refusal. File mode is validated before startup, so a mistyped value such as `O600` cannot silently fall back to a permissive umask. Accepted sockets carry a write timeout covering every response, and a second descriptor permits `shutdown` without waiting behind a blocked writer lock. A half-written notification is not retried: the framing has no resynchronisation point, so the peer is disconnected and may reconnect.

On rebootstrap, merely sending `UNAVAILABLE` left peer-held `Snapshot` values pinning `DbInner` and its directory lock. `Channel::disconnect_peers` revokes them; the follower empties the database slot **before** disconnecting, so newly admitted peers cannot acquire more old snapshots during the gap. At full shutdown, the channel closes before waiting for readers. Socket existence is not daemon readiness: the listener may be bound before the database is open and answer `UNAVAILABLE`.

### Encoding and test lessons

The stored Roaring run pair is `( start, len_minus_1 )`; the published plugin payload is `( start, end )`. Passing stored bytes through gave plausible but short intervals when `start <= len_minus_1`, and an empty reversed interval when `start > len_minus_1`. A fixture beginning at zero made the old answer accidentally correct. Both arena and inline encoders convert; the removed in-process C table had to copy runs through scratch rather than lend stored bytes. Tests must compare decoded contents with a nonzero run start, not only kind and interval count.

The first tests of the inline capacity used sparse lanes and never approached the frame bound. The admission test connected sequentially and could not expose simultaneous overflow. A test of blocked-writer hang-up never made a serving thread hold the writer lock. Each was replaced by a fixture that makes the failed mechanism observable. A real daemon smoke test also found that the adopted metrics surface retained an `Arc<Db>` across clean `SIGTERM`, causing exit status 1; `Shared::release()` returns it to `Starting` before teardown.

### The dense write wire

The read side gained a dense wire on 2026-09-30; the write side still carried **one row per set
bit** until 2026-10-03. Measured on a 64-chunk half-dense page -- 2 097 152 ordinals over 512 KiB
of actual bitmap:

| wire | bytes | per set bit | vs. the data |
|---|---:|---:|---:|
| pair `{ key, ordinal }` | 34 126 004 | 16.3 | 65x |
| container payloads | 526 948 | 0.251 | 1.005x |

**64.8x on the wire, and no new encoder or schema.** `apply` and a write transaction accept a
batch carrying the *same* `containers_schema` the read side already ships, payloads byte-identical
to the page store and to a `.roaring` file, and each row becomes
`patch_chunk( key, prefix, empty, c )` -- which is `( old \ {} ) union c`, exactly what `insert`
means. Because it is an encoding change to an existing operation, the correctness claim is one
differential test against the wire that already worked rather than a round trip proving the new
encoding agrees with itself.

End to end, both wires landing an identical set, verified ordinal by ordinal:

| page | pairs | containers |
|---|---:|---:|
| 64 chunks, 512 KiB | 1058-1212 ms | 41.0-42.1 ms |
| 1024 chunks, 8 MiB | 16.6-17.1 s | 29.2-48.5 ms |

**The container arm is flat across a 16x size range**, which is the real shape of the result: it
is no longer wire-bound. The larger page measuring slightly faster than the smaller one is
unexplained and was left so, because both are orders of magnitude under the pair arm and an
unverified cause is worth less than saying it is unverified.

### A bound that refused the case the feature existed for

A container row reports the **ordinals** it carries, so the same logical write reports the same
figure on either wire and the acknowledgement check compares encoded against decoded cardinality.
That is right for reporting, and wrong as an input to `MAX_TRANSACTION_ROWS`, which exists to
limit **memory** in rows because a mutation row costs a few dozen bytes. The 8 MiB page was
therefore refused with `apply exceeded 16777216 staged rows` -- the bound rejecting precisely the
case the dense wire exists to make possible -- while the 64-chunk fixture passed every time.

The repair separates three quantities that had been two: ordinals written ( reported ), operations
staged ( bounded by `MAX_TRANSACTION_ROWS` ), and container payload bytes behind them ( bounded by
`MAX_TRANSACTION_CONTAINER_BYTES`, 256 MiB ). **Any one standing in for another lets a bound
refuse work it was not meant to refuse, or admit memory it was meant to refuse.**

`a_dense_page_past_the_row_bound_in_ordinals_is_still_accepted` pins it at 257 full chunks, one
past the bound, which is affordable only because `Container::full()` expresses a whole chunk as
one run interval -- 16 777 217 ordinals in a few hundred bytes rather than a 134 MB vector.

**The fixture that was convenient to build was also the one that could not fail.** A working
feature, a passing differential test and a 64.8x wire figure all existed before anyone measured at
the size the backlog actually named, which is where the headline case returned an error.

### The advertised cap was not the protocol ceiling

A write cap was published as a protocol limit and was **6.25x** smaller than what the protocol
actually permits -- the advertised figure, not the ceiling. The same file already recorded this
exact failure once before. Raising it is worth 5.6x on writer time with checkpoint unchanged, but
the absolute numbers belong to a particular setup and the ratio is setup-dependent; **no claim is
made that the cap alone is a speedup.**

### Reviewing two implementations of one protocol

A differential review of the two channel clients found four defects in one and one in the other,
and the method matters more than the count: **agreement between two implementations is not
evidence**, because both can encode the same misreading. Two findings generalize --

- A client must **read on through a database replacement** rather than treating it as the end of
  the stream.
- **A protocol's error table belongs beside its interpreter.** The architectural finding was
  where that knowledge was living: duplicated in each client rather than owned by the protocol.

### Where the C ABI lives, and what the header may not be called

The channel's C ABI belongs in `yesno-plugin`, which already publishes one; the header **must
not** be named `yesno_plugin.h`, because the two would collide for any consumer linking both. The
peer half was later split out so a host embedding only the peer does not drag the host half's
dependencies in. Local pre-checks were weaker than the gate three times over that work, which is
the recurring argument for running the gate rather than reasoning about it.

## Files

- `yesno-plugin/src/ipc.rs`: frame codec and structural bounds.
- `yesno-plugin/src/channel.rs`: sessions, arenas, descriptors, and socket serving.
- `yesno-server/src/plugin.rs`: channel configuration, admission, access, and lifecycle integration.
- `yesno-server/tests/plugin_channel.rs`: real Unix socket and arena protocol tests.
- `yesno-server/tests/daemon_smoke.rs`: binary-level startup and shutdown path.
- `.agents/docs/LTM/operator-hosted-plugin-container-plan.md`: sidecar and operator deployment constraints.

## Test Coverage

Codec tests check every prefix of every frame returns `Truncated` and arbitrary bytes across kind values do not panic. Session tests cover handle lifetime, lane reuse, paging, inline/arena equivalence, and the 265-lane split over one snapshot. Server tests use a real socket and descriptor transfer, race twelve connects against a cap of three, test credentials and guarded binding, and prove a silent peer is disconnected for rebootstrap. The response-writer regression holds the lock and checks hang-up with a deadline; a separate-process peer test checks socket-death liveness.

Run `cargo test -p yesno-plugin` and `cargo test -p yesno-server`; the operator scenario additionally exercises the sidecar path.

## Pitfalls

- A protocol feature tested through `Session::handle` may still be unreachable from `yesnod`; test the daemon entry point and a real socket.
- Never hold the peers mutex or response writer lock across an unbounded socket write, and never require that lock to disconnect a peer.
- A notification is not revocation. Peers may stop reading; close their sockets to release snapshots before replacing the database.
- Do not equate a socket pathname or a configured mode with the authenticated peer uid. Validate both.
- Do not expose stored run pairs under the inclusive-end wire contract. The copy required to convert them is part of correctness.
