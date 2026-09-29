# Backup and archiving

This guide covers independent backups: one-off base backups, and continuous
archiving to an object store with retention. How a consistent snapshot is
captured is in [Snapshot providers](snapshots.md); getting data back is in
[Restore](restore.md).

Replication does not protect against an accidental delete, bad migration, or
application bug. Take independent backups.

The [on-disk format reference](../storage-format.md) describes which files carry
recoverable state and how their A/B slots, checksums, and WAL replay positions
are interpreted.

## Base backups

The backup and archive clients are built from their independently deployable
utility package:

```console
cargo build --release -p yesno-server-utils
```

Use `yesnoctl basebackup` against the authenticated shared control endpoint:

```console
yesnoctl basebackup \
  --endpoint https://leader.internal:50052 \
  --ca /etc/yesno/replication-ca.pem \
  --cert /etc/yesno/backup-client.pem \
  --key /etc/yesno/backup-client.key \
  --server-name leader.internal \
  --target /backups/yesno-2026-08-30
```

The target must not exist. `yesnoctl basebackup` uses only the control-plane
base-snapshot protocol: it acquires an immutable lease, streams every named
file while renewing that lease, and releases it when transfer finishes. The
client never reads the server data path and never asks the replication service
for shard images or WAL. It validates and replays the staged copy with ordinary
replica recovery, synchronizes it, and publishes the target with one directory
rename.

The completion line records the database UUID, leadership term, shard count,
base checkpoint, recovered version, byte size, and attempt count. Store the
completed directory outside the leader and standby failure domain.

The manifest is mandatory. It carries the database identity and routing map.
Restoring shard files without it can create a new identity and route keys to the
wrong shards.

If the shared control endpoint is unavailable, take a cold copy instead: stop
`yesnod` cleanly, copy `MANIFEST` and every `shard-NNNN.yno`, then restart.
The final shutdown checkpoint makes WAL unnecessary for that cold procedure.

## Continuous archiving

A base backup is one recoverable point. For a sidecar on the database host,
grant only its local connection channel the two archive capabilities:

```toml
[server.control]
unix_socket = "/run/yesno/control.sock"
journal_dir = "/var/lib/yesno-control"

[[auth.rule]]
channel = "local"
principal = "all"
capability = "control-read"
action = "allow"

[[auth.rule]]
channel = "local"
principal = "all"
capability = "replication"
action = "allow"
```

Restrict the socket's parent directory and socket mode to the daemon and
sidecar accounts. The ordered rules still limit what an admitted local process
can call. Then run the sidecar through that socket:

```console
AWS_REGION=ap-northeast-1 \
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive
```

A fresh archive publishes no base merely because the sidecar started. It waits
for a completed checkpoint event, including one replayed from the durable
control journal, and uses that event as the first base-generation trigger. Run
`yesnoctl checkpoint` after first deployment if an immediate baseline is
required. Once that recovery root exists, later ordinary checkpoint events only
advance the durable event cursor; they do not replace the base. If the requested
event range has already been compacted, a fresh archive conservatively takes a
base, while an established archive keeps its existing recovery root and advances
to the journal's state snapshot.

An archive process on another host instead uses the TCP listener, a dedicated
mutual-TLS identity, and `hostssl` rows for the same two capabilities.

`file:///srv/yesno-archive` selects a local filesystem store. Standard AWS
environment variables configure S3 credentials and region; `AWS_ENDPOINT`
selects an S3-compatible service. The sidecar writes Protobuf `state.pb` and
base manifests, never JSON. Data objects are below a database UUID and
leadership-term prefix, so a promoted timeline cannot overwrite an older
timeline's identically numbered WAL.

Writer fencing and history commitments are archive schema version 2. A version
1 prefix is rejected because it has no trustworthy fingerprint from which to
continue. Start version 2 in a new object prefix and retain the old completed
bases according to the previous recovery policy; do not fabricate a version 2
`state.pb` over version 1 objects.

The default `auto` archive mode asks `yesnod` for a control-plane snapshot
lease. With the default server configuration, `yesnod` makes a portable staged
copy; ZFS or Btrfs replaces that copy with a filesystem-native snapshot behind
the same protocol, and LVM supplies a locally mounted classic snapshot LV.
`--snapshot-mode network` explicitly selects the archive sidecar's replication
fallback. Each provider is configured as described in
[Snapshot providers](snapshots.md).

## WAL archiving and the writer lease

WAL bytes come from the replication stream, not from WAL-generation lifecycle
events: a sealed generation may already have been reclaimed when an event is
observed. For each batch the sidecar writes the WAL object, atomically advances
remote and local Protobuf state, and only then ACKs the LSN. That ACK is also
the leader's retention floor. If forced retention moves past an offline
sidecar, it automatically takes a new base before resuming. Ordinary checkpoint
events do not take a new base, because doing so would discard older recovery
coverage. A leadership-term advance clears the old timeline and takes a new
base when the sidecar restarts. Configure the object store to expire abandoned
multipart uploads, because process or host failure can prevent an in-progress
upload from being explicitly aborted.
An abrupt sidecar exit does not orphan a snapshot indefinitely: it renews the
server lease while uploading, and `yesnod` destroys an unrenewed lease after
`lease_ttl_secs` (see [Snapshot providers](snapshots.md)). A server shutdown
also releases all leases. If the server itself crashes, its next successful
database open reconciles snapshots left in that database's namespace.

Run one sidecar writer for an object-store prefix. It owns a renewable
`writer.pb` lease; remote acquisition, renewal, and `state.pb` publication use
conditional object updates, while `file://` uses a kernel file lock. A stale
writer cannot publish after a replacement fences it. The backing object store
must honor conditional create and update operations. The default lease is 30
seconds and can be changed with `--writer-lease-ttl-secs`.

Every WAL object has an immutable Protobuf descriptor and a SHA-256 history
link rooted in its base manifest. On subscription and reconnect, the sidecar
rereads WAL from that base and compares it with the archived chain before
appending. A same-term divergent endpoint is rejected; if retention has removed
the verification root, the sidecar takes a new base. This adds reconnect
bandwidth. It does not replace external leader fencing, which is still required
to prevent split brain rather than merely detect it at the archive boundary.

## Retention and reclamation

Bases, WAL objects and their descriptors are immutable and are never rewritten.
Nothing is deleted unless the sidecar is told what to keep:

```console
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive \
  --retention-window 7d
```

The window is a promise about recovery points, not about object age: **every
wall-clock instant within it stays restorable.** A pass keeps the newest base at
or below the start of the window -- the root a restore to that instant would
select -- every base after it, and every WAL object those roots still reach.
Accepted units are `s`, `m`, `h` and `d`; a bare number is refused, because every
wrong guess about the unit deletes recoverable history.

Omitting `--retention-window` disables reclamation entirely, which is the
default. `--retention-min-bases` (default 2) keeps that many newest bases
regardless, so a database quiet for longer than its window still has a root.
`--retention-interval-secs` (default 3600) sets the pass interval.

A pass never deletes the active base, a base whose commit time is unknown
(published before commit times were recorded, so it cannot be placed in the
window at all), the marker of an interrupted rebase, or any object whose key it
does not recognize. Unrecognized objects are counted in the pass summary; a
non-zero count means the archive holds objects this version does not understand
and reclamation is incomplete until that is explained.

Every reference is removed before the thing it names, so an interrupted pass
leaves unreferenced objects that the next pass finishes off, never a manifest
pointing at files that are gone.

**Object-store lifecycle rules are not a substitute.** A rule that expires
objects by age does not know that a WAL object from last week is anchored to a
base from last month, and will delete the base while keeping WAL that can no
longer be replayed. Use lifecycle rules only to expire incomplete multipart
uploads.

Reclamation runs under the same writer lease as publication, renewed immediately
before each pass, and is abandoned if the archive state moves while it is
planning. Run it from the same single sidecar that owns the prefix.

### Reclamation metrics

The pass summary is otherwise only a log line, so a deployment that reclaims
should scrape the sidecar. `--metrics-addr` (or `YESNO_ARCHIVE_METRICS_ADDR`)
serves `/metrics` and `/healthz` on that address:

```console
yesno-archive \
  --endpoint unix:///run/yesno/control.sock \
  --store s3://yesno-backups/production \
  --work-dir /var/lib/yesno-archive \
  --retention-window 7d \
  --metrics-addr 127.0.0.1:9760
```

The listener is off by default, unlike the daemon's, because several sidecars
are routinely run on one host, one per archived database, and a default port
would make the second one fail to start on a bind nobody asked for. Bind it to
loopback: like the daemon's, it is unauthenticated. There is no `/readyz`;
nothing sends traffic to a sidecar, so there is no readiness question to answer.

The counters accumulate over the sidecar's lifetime and reset when it restarts:

- `yesnod_archive_reclamation_passes_total`: passes that completed and produced
  a plan.
- `yesnod_archive_reclamation_failures_total`: passes that ran and failed. A
  failed pass is retried on the next interval and never stops archiving, so
  this counter is the signal that reclamation is not keeping up.
- `yesnod_archive_reclamation_skipped_total`: passes skipped because the writer
  lease was not renewed. Nothing was attempted, which is why it is not counted
  as a failure.
- `yesnod_archive_objects_deleted_total`: archive objects deleted.

The gauges report the last completed pass. They are absent until one completes,
so a sidecar that has never surveyed the archive publishes no reading rather
than a reassuring zero:

- `yesnod_archive_unrecognized_keys`: objects the last pass could not classify,
  and so neither retained deliberately nor deleted. Alert on any non-zero
  value. It means the archive holds objects this version does not understand,
  that nothing will ever reclaim, and that the store grows without bound while
  every other signal looks healthy. The usual causes are a newer writer's
  objects under the same prefix and hand-placed files. Identify them before
  assuming reclamation is complete, and do not delete them by hand until you
  know what wrote them.
- `yesnod_archive_retained_bases`: base images the last pass kept. Read it
  against `--retention-min-bases` and the window.
- `yesnod_archive_reclamation_last_success_timestamp_seconds`: Unix time of the
  last completed pass, and therefore how old the two gauges above are.

A failed pass deliberately leaves the gauges at their last completed reading. A
pass that fails learns nothing, and zeroing the unrecognized count because the
pass failed would replace an absence with a false claim. Alert on staleness
instead: a freshness stamp more than one interval behind, or a rising failure
count, means the gauges no longer describe the archive.
