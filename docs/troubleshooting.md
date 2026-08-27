# Troubleshooting

This guide starts from symptoms visible to a user or operator. Run commands
against the exact endpoint and credentials used by the failing application;
testing an unauthenticated loopback endpoint can hide a remote TLS or role
problem.

## First checks

Validate configuration without opening the database:

```console
yesnod --config /etc/yesno/yesnod.toml --check-config
```

Then ask the running endpoint what it is willing to do:

```console
yesno --endpoint https://yesno.internal:50051 \
  --ca /etc/yesno/tls/ca.pem \
  --token-file /run/secrets/yesno-token \
  status
```

`status` shows the resolved identity, leadership term, available actions, and
space counters. Check `/healthz` for process liveness and `/readyz` for readiness
to serve the configured workload. A follower rebuilding its local image can be
healthy without being ready.

## The client cannot connect

Work outward from the error chain printed by `yesno`:

1. Confirm the endpoint scheme, host, and port. Plain local service defaults to
   `http://127.0.0.1:50051`; a TLS endpoint uses `https://`.
2. Confirm that the server is listening on the expected interface. A loopback
   listener is not reachable from another host or container.
3. If a certificate names a DNS host but the client connects by IP address, set
   `--server-name` to the certificate name.
4. Confirm that `--ca` names the CA that signed the server certificate, not the
   server certificate itself unless it is intentionally self-signed.
5. For mutual TLS, supply both `--cert` and `--key` and confirm that the server
   trusts their issuer.

Certificate files are read at process start. After replacing one, restart the
server before retesting.

## Authentication or permission is denied

`UNAUTHENTICATED` means the server could not establish the caller's identity.
Check that the token file contains only the intended bearer token, that the
configured SHA-256 digest is lowercase hexadecimal, and that a client
certificate and token do not identify two different principals.

`PERMISSION_DENIED` means identity succeeded but authorization refused the
operation. Flight uses the reader/writer/admin role hierarchy. The shared
control endpoint instead uses ordered `[[auth.rule]]` rows for `control-read`,
`control-admin`, and `replication`; the first match decides and no match denies.

Do not put a bearer token directly in process arguments. Use `--token-file` or
the corresponding environment variable so it does not appear in shell history
or the process list.

## The database says it is already open

Only one process may open a data directory. Stop the old process and wait for it
to release the lock before starting its replacement. Do not run overlapping
instances against one volume as a rolling restart strategy.

If no process appears to be running, inspect the service manager and container
runtime before assuming the lock is stale. The lock is held by an open file
descriptor, not by a marker file that should be deleted.

## A write is rejected on a follower

A follower is read-only by design. Send the write to the leader. Promote the
follower only as part of the fenced failover procedure; accepting one rejected
write is not sufficient reason to promote a node.

After a promotion, record the new leadership term and pass it to clients with
`--min-term`. A client that reaches a superseded leader will then fail instead
of accepting a write on the obsolete timeline.

## A query count succeeded but row retrieval failed

Flight query planning returns a ticket naming a database version, then row
retrieval uses that ticket. If checkpointing reclaimed the version between
those steps, repeat the original query to obtain a fresh ticket. Do not retry
the old ticket indefinitely.

Also confirm that the ticket went back to the same node. A replica can be behind
the leader, and independently querying two endpoints does not provide one
shared snapshot.

For large or complement-heavy queries, prefer `--count-only` when possible. If
you only need a sample, use `-n LIMIT`; otherwise the client may legitimately be
streaming an enormous result rather than hanging.

## A snapshot was evicted

The database can invalidate an old reader to bound space amplification. The
error identifies the snapshot version. Restart the unit of work at a fresh
snapshot and shorten how long snapshots remain open.

Already materialized data remains valid, but future reads through the evicted
snapshot fail. Applications performing long scans should keep a restart point
in their own domain, such as the last completely processed key.

If eviction is frequent, inspect reader age and space counters before changing
policy. A forgotten snapshot, long-lived query-engine source, or stalled client
is often the cause.

## Disk use or WAL size keeps growing

Run:

```console
yesno stats
yesnoctl checkpoint
```

Checkpointing makes committed state durable in the shard images and permits old
WAL and unreachable extents to be reclaimed. In embedded use there is no
background checkpoint thread; the host application must schedule checkpoints.

If space remains pinned after a checkpoint, look for old snapshots and lagging
replicas. Reclamation must preserve bytes still reachable by a reader, and a
leader must retain WAL needed by a follower. Do not delete WAL or shard files by
hand.

## A replica does not catch up

Check the follower's readiness and replication status, then verify network
reachability, TLS credentials, and a matching `replication` authorization rule
on the leader's shared control endpoint. Rules match the immediate TCP peer
address; behind a proxy they see the proxy address, not a forwarded client
address.

A follower cursor can become unusable after the leader cuts its WAL. The safe
recovery is a fresh physical bootstrap, not guessing a new byte offset. Keep the
old leader fenced throughout any promotion or rebuild.

Remember that an asynchronous follower can be healthy and still be stale. It
must not serve read-after-write traffic that requires the latest leader commit.

## Ingest reports an input error

`yesno put` expects one `key,ordinal` pair per non-empty line. Lines beginning
with `#` are ignored. Both fields must be decimal unsigned 64-bit integers, and
the ordinal must not equal `u64::MAX`.

```text
# key,ordinal
42,1
42,5
7,5
```

The command rejects an entirely empty input. It inserts memberships only; it
does not interpret a second occurrence as a removal.

## Results are surprising

Check the model before assuming corruption:

- A missing key is an empty set.
- `range(lo, hi)` filters ordinals and excludes `hi`; it does not select keys.
- `and-not(a, b)` is directional.
- `not(a)` is relative to the full valid ordinal universe, not to the set of
  ordinals currently present under any key.
- `-n LIMIT` limits printed rows but does not change the exact count.

Run the component keys separately with `count` and inspect a small result with
`get -n`. Then add operators back from the inside out. The
[query-language reference](query-language.md) defines every operator.

## Suspected on-disk damage

Stop writes and preserve a copy of the data directory before experimenting.
Do not create a new manifest beside existing shard files, combine files from
different backups, or delete a file merely because its name resembles a log.

Restore a complete checkpointed backup into an empty local directory and
validate known keys there. The manifest and every matching shard image belong
together.

A local data directory cannot be recovered to an arbitrary point of its own WAL.
Point-in-time recovery is a property of a published archive: recover from that
instead, targeting a commit version or a wall clock, and let the restore assemble
a directory rather than assembling one by hand.

An mmap I/O failure on a network filesystem may terminate the process rather
than return an ordinary database error. yesnodb requires a supported local
filesystem; move the data to one before further testing.

The [operations guide](operations.md) owns the complete backup, restore,
replication, and promotion procedures.
