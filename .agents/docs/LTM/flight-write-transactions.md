# Flight Write Transactions and Mixed Mutation

Answers the handoff at
`.agents-workspace/tmp/yesno-cdc-flight-ticket-handoff-2026-09-23.md` and its
haiiie addendum, then records what was built. Verified against `main` at
`85a41c2` before any change.

## The contract that existed: reads only

**No write transaction existed anywhere.** Not in a branch, a design note, or
a client surface:

- `Ticket` is `{ version, key, prefix_lo, prefix_hi, expr_hash, expr }`. Its
  own header says the version field exists so several endpoints can answer
  from the *same* snapshot. Every field is about reading.
- The action surface was `stats`, `clear`, `contains`, `insert_one`,
  `remove_one`. All single-shot.
- `do_put` never received or validated a ticket.
- No occurrence of transaction, txn, begin, prepare or two-phase in
  `yesno-flight` or `yesno-wire`.

**The atomicity unit was one Arrow record batch.** `do_put` created a
`WriteBatch` per record batch and committed each. `client::put_one_batch`
therefore gave one commit spanning many keys -- and the client documentation
already drew the distinction exactly: *"one batch is one commit, so the version
names exactly this call's write. The streaming forms commit per batch and
report the last version, which is a state containing every row sent but not a
single atomic instant for them."*

So the missing capability was narrower than the handoff assumed. Not "many
rows" and not "many keys"; those worked. **Only mixing operations, and only
spanning requests.**

## The capability was already in the core

`WriteBatch` accepts `insert`, `remove`, `insert_range`, `remove_range` and
`delete_key`, mixed, across arbitrary keys, in one commit. `do_put` itself
called `wb.insert` and `wb.remove` on the same batch object. Only the wire
could not say so.

**And order within a key is preserved, which is what makes mixing correct.**
Commit sorts through a `( key, arrival index )` pair array, and the core
records why: *"Stability is structural here rather than a property of the
algorithm... so `sort_unstable` is correct and the guarantee cannot be lost by
someone later swapping the sort for a faster one."* The test
`interleaving_order_does_not_change_what_a_batch_commits` pins it.

That is the whole correctness argument for `delete_key` followed by inserts
meaning replacement.

## Answers to the handoff's questions

1. **Does a write ticket exist?** No. See above.
2. **Overload the ticket or a separate handle?** Separate, and it is now
   `WriteTxn`. A read ticket names an immutable version and prefix range and is
   safe to cache, copy and replay across endpoints; a write handle owns mutable
   staged state with an expiry and exactly one resolution. Sharing an encoding
   would not make their lifecycles the same, and would turn every cached ticket
   into a potential mutation capability.
3. **Can several `DoPut` streams become one `WriteBatch` and one version?**
   Yes, implemented. A transaction holds a live `WriteBatch` -- it owns a
   refcounted `Db` handle and is therefore `'static` -- and every staged stream
   appends to it. Commit is one core commit and one version.
4. **Begin, commit, abort, expiry, size limits, cleanup?** All implemented:
   `begin_write` / `commit_write` / `abort_write`, a deadline per transaction
   swept on begin, `MAX_OPEN_TRANSACTIONS`, `MAX_TRANSACTION_ROWS`, and
   refusal of a second concurrent stream on one handle.
   **Ownership and leadership fencing are not implemented, and cannot be**:
   the Flight service authenticates nobody, so a handle is bound to whoever
   holds its eight bytes. Acceptance item 5 of the addendum is therefore
   unmet and is named as unmet rather than tested vacuously.
5. **Idempotent commit?** Yes. A bounded memory of `( transaction, version )`
   means a retry after a lost response returns the original version instead of
   applying the work twice. Aborting an already-committed transaction is an
   error, because a version exists that contradicts the caller's belief.
6. **In memory, spilled, or durably prepared?** In memory, bounded, and
   deliberately neither of the others. CDC needs atomic *application*, not
   distributed atomicity with a source that has already committed. A durable
   prepare would only be needed to close the PostgreSQL/MySQL commit window,
   which is a separate protocol and a separate decision.
7. **Can the drivers collapse their sequences?** Yes, and neither has been
   changed yet. `yesno-pg`'s `flush()` partitions by endpoint and key then
   calls `Transport::put` once for removals and once for insertions;
   `yesno-mysql`'s Flight backend issues `Clear` per key, then `RemoveMany`,
   then `InsertMany`. Both become one staged transaction. Their savepoint
   buffers and read overlays sit above this layer and are untouched.

## What was built

- `mutations_schema()` -- `( key, lo, hi, op )`, one row per `WriteBatch`
  operation, covering point insert/remove, **inclusive range** insert/remove,
  and whole-key delete. Ranges travel as ranges: the engine writes one record
  and one container call per chunk, and expanding client-side throws both away.
- `PUT_APPLY` -- mixed operations as one commit, without a handle. The
  one-shot form for work that fits in one request.
- `PUT_TXN_PREFIX` plus `begin_write` / `commit_write` / `abort_write`.
- `ServerStats::features` and `YesnoClient::supports`.
- Client: `Mutation`, `WriteTxn`, `begin_write`, `stage`, `commit_write`,
  `abort_write`, `apply`.

### The hazard that had to be cleared first

`do_put` used to treat an **absent or unrecognised** command as insert. That
made every future command a trap: a client sending `apply` to a server that
did not know it would have had its *removals applied as insertions*, with no
error anywhere.

A descriptor command is now **required**, and absent, empty and unrecognised
are all errors. The two callers that relied on the default -- the `yesno put`
CLI and the e2e harness -- now name `insert` explicitly; the Go and C++
clients always did. This is a wire break, taken deliberately because nothing
has been released publicly.

### Concurrency rule

**One staging stream at a time per transaction.** Operations take effect in
the order they were recorded, so the order has to be one the caller can
predict, and arrival order across two HTTP/2 streams is not. The alternative
-- per-stream sequence numbers with commit rejecting gaps -- buys concurrency
nobody has asked for.

## The fixture that discriminates, and the one that did not

The addendum warns that *"replaying operations grouped by kind would fail this
fixture while still appearing atomic"*. The first version of the order test
**did not catch it**: every case it contained had removals *before*
insertions, which is exactly what grouping by kind produces anyway. Mutating
the server to group by kind left it green.

The discriminating case is a removal that *follows* an insertion in one batch:
`Insert` then `DeleteKey` on a key must leave it empty, and `Insert` then
`Remove` of one membership must leave it absent. Grouping inverts both. With
those added, the mutation fails the test with exactly the message that
explains why.

## Documentation found stale

`yesno-mysql/README.md` and `database-apis-and-satellite-crates.md` describe a
nontransactional engine that advertises `HA_NO_TRANSACTIONS` and registers no
commit hooks. `ha_yesno.cc` says *"lets this engine stop advertising
`HA_NO_TRANSACTIONS`"* and implements savepoints. **The code moved and the
documentation did not.** That answers the handoff's open question; the
documents still need correcting.

## Read tickets need no change

The haiiie probe confirmed current tickets pin a read snapshot exactly. Its
separate finding -- that one `DoGet` per key/block is unsuitable as a scoring
transport -- is about a remote block-read protocol and is deliberately not
coupled to this work. Write atomicity and scoring transport are different
questions.
