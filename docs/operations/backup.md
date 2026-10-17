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
continue. Neither storing a base object without its empty regions nor splitting
a base file into content-named pieces advanced that number: both add fields to a
manifest rather than changing what the existing ones mean, so every archive
already in a store still restores unchanged, and the history fingerprints --
which commit to a file's length and checksum, both still the whole file's -- are
untouched. The compatibility that does not hold is the other direction. A
release older than this one, pointed at a base stored without its empty regions,
refuses it by object length; pointed at one split into content-named pieces, it
fails to find an object to fetch. Both refuse rather than restoring something
wrong, but neither reads a base this release writes. Start version 2 in a new object prefix and retain the old completed
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

A retained base generation costs what its data occupies, not what a shard image
appears to be, and successive generations cost only what changed between them.

A shard image is a sparse file whose apparent length is a whole memory-mapping
segment, currently one gibibyte, whatever the database holds. An upload cuts it
into fixed-size pieces, skips every piece that is entirely empty, and names each
of the rest by a hash of its contents. Measured on a four-shard database holding
a few hundred kilobytes: **5.01 MiB of base objects against an apparent
4.00 GiB**. A piece that carries anything is stored whole, so the saving from
skipping is whatever fraction of the image is empty.

Naming a piece by its contents is what makes a second base generation cheap. A
checkpoint rewrites only the pieces it touched, so the rest already exist in the
store under exactly the names the new manifest gives them and are not sent
again. A generation therefore costs roughly what changed since the last one,
rather than the whole database each time. Objects are shared between
generations, and a piece is removed only once no retained generation names it.

Weigh that against how often a new base is rooted and how many generations the
window retains -- but note that the second generation of a quiet database is
nearly free, which was not true before.

The window is a promise about recovery points, not about object age: **every
wall-clock instant within it stays restorable.** A pass keeps the newest base at
or below the start of the window -- the root a restore to that instant would
select -- every base after it, and every WAL object those roots still reach.
Accepted units are `s`, `m`, `h` and `d`; a bare number is refused, because every
wrong guess about the unit deletes recoverable history.

Omitting `--retention-window` disables reclamation entirely, which is the
default. It does not switch the pass off: see the survey below.
`--retention-min-bases` (default 2) keeps that many newest bases regardless, so
a database quiet for longer than its window still has a root.
`--retention-interval-secs` (default 3600) sets the interval for both.

A pass never deletes the active base, a base whose commit time is unknown
(published before commit times were recorded, so it cannot be placed in the
window at all), the marker of an interrupted rebase, or any object whose key it
does not recognize. Unrecognized objects are counted in the pass summary; a
non-zero count means the archive holds objects this version does not understand
and reclamation is incomplete until that is explained.

A base manifest whose *contents* this version cannot read is a stronger case
than an unrecognized key, and it stops the pass deleting anything at all. Every
deletion is justified by what the manifests name, and one that will not decode
could name any object in the store, so there is no safe subset to fall back on.
The pass still runs and still reports; it names the object and the reason, and
`yesnod_archive_unreadable_base_manifests` goes non-zero. The realistic cause is
a writer from a newer release sharing the prefix, which is why a new archive
schema version belongs in a new prefix.

Every reference is removed before the thing it names, so an interrupted pass
leaves unreferenced objects that the next pass finishes off, never a manifest
pointing at files that are gone.

**Object-store lifecycle rules are not a substitute, and since base files are
stored as content-named pieces they are actively dangerous.** A rule that
expires objects by age does not know that a WAL object from last week is
anchored to a base from last month, and will delete the base while keeping WAL
that can no longer be replayed. Worse, a piece of a base file is named by its
contents, so a later base that still contains that piece **names it instead of
uploading it again** -- which is the whole point, and it means the object's
last-modified time stops advancing while the object stays in use. Age is
therefore no evidence at all about whether a piece is still needed, and a rule
expiring objects after 90 days will delete live pieces of the base being
written today. Use lifecycle rules only to expire incomplete multipart uploads.
`yesnod_archive_missing_objects` reports the damage if one has already run, and
`yesnoctl archive-plan` answers the same question on demand.

**One reading says how much time is left, rather than what has already gone.**
A pass knows which objects the archive still needs, and the listing it decided
that from carries each object's last-modified time, so it also reports the
oldest object it is keeping: `yesnod_archive_oldest_live_object_timestamp_seconds`,
and the `oldest object still needed` line of `yesnoctl archive-plan`. An expiry
rule destroys live data the moment that value passes the rule's age, so alert on
`time()` minus the gauge against whatever the rule is set to, with enough margin
to act. It is deliberately not the age of the oldest object in the bucket: an
archive always holds superseded bases and spent WAL older than anything it
needs, and those are exactly what a rule is welcome to take. Copying an archive
resets it, because every object an `rsync` or an `aws s3 sync` writes is new --
that is the truth about the copy rather than an error in the reading.

**The sidecar surveys the archive whether or not reclamation is configured.**
A sidecar with no `--retention-window` runs the same pass on the same interval,
reports what it found and deletes nothing, so an archive that is only ever
written to still publishes whether it is still whole. This is deliberate: the
deployments with reclamation off are the ones most likely to be leaning on a
bucket lifecycle rule, and before this they were the ones with no reading to
alert on. A survey costs one listing and one GET per base manifest per
interval. `yesnod_archive_objects_deleted_total` stays flat, which is how a
dashboard tells the two modes apart.

Reclamation runs under the same writer lease as publication, renewed immediately
before each pass, and is abandoned if the archive state moves while it is
planning. Run it from the same single sidecar that owns the prefix.

### Previewing a pass, and checking an archive is whole

`--retention-window` is irreversible in effect, so look before setting it:

```console
yesnoctl archive-plan \
  --store s3://yesno-backups/production \
  --retention-window 7d
```

It reports what a pass with that policy would delete, what it would not
recognize, what a base it would keep names that the store does not hold or
holds at a length the manifest does not claim, and which bases those losses
cost. It **deletes nothing** and takes no writer
lease, so it is safe to run against a live archive and alongside the sidecar
that owns the prefix. Each list is sampled; `--list` prints every key.

Read the damaged-base list before the missing-object list. They are the same
damage in different units, and the object count does not give the severity: a
base file is stored as content-named pieces that later generations name
instead of uploading again, so one lost piece can cost every generation in the
archive, while a hundred lost objects can all belong to one old base nobody
would restore. The key of a lost piece is a hash and says nothing about which
generations named it, so this is not an attribution to do by hand.

The last line of the report is the oldest object the archive still needs, with
its last-modified time. That is what a bucket lifecycle rule has to be set
longer than, and it prints on a healthy archive too -- on the archive an
operator is about to put a rule on, every list above it is empty.

It exits **2** when the archive is not whole -- any missing object, any object
of the wrong length, or any base manifest this build cannot read -- and 0
otherwise, so a scheduled check
can act on the verdict without parsing the output. An exit of 1 means the
command itself failed: an unreachable store, a bad endpoint, a typo. An
unrecognized key is deliberately not part of the verdict; it is worth looking
into and is not a reason for a nightly check to go red.

Omit `--retention-window` and the plan is computed against a window no base
falls out of. Nothing is then proposed for deletion and the report is purely
about what the archive holds -- which is how to ask "is this archive still
whole" on the default configuration, where reclamation is off and no pass is
publishing the gauges below. Run it after any change to bucket lifecycle,
replication or access policy.

A capture running at the same time can make the deletion count optimistic,
because objects uploaded after the listing are not in it. It can also, in one
narrow case, report an object as missing that is in fact there: a listing is
served in key order, and a capture can store a piece after that part of the
listing was served and publish the manifest naming it before the manifests
were read. Re-run the command if it reports a loss while a base copy is in
progress. The sidecar's own survey excludes this by never running at the same
time as a capture; the command does not, because waiting its turn would hold
the sidecar off its own archive.

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
  a plan. A sidecar with reclamation off counts its surveys here too; they are
  what keeps the gauges below fresh.
- `yesnod_archive_reclamation_failures_total`: passes that ran and failed. A
  failed pass is retried on the next interval and never stops archiving, so
  this counter is the signal that reclamation is not keeping up.
- `yesnod_archive_reclamation_skipped_total`: passes skipped before anything
  was attempted, which is why they are not counted as failures. Two causes: the
  writer lease was not renewed, or a base capture was in progress. The second
  is routine on a database whose base copy takes longer than the reclamation
  interval -- reclamation and capture are deliberately never run at the same
  time, because a capture's objects are not yet named by any published
  manifest. A steady climb with no completed pass between the skips means the
  interval is shorter than the capture, and the interval should be raised.
- `yesnod_archive_objects_deleted_total`: archive objects deleted. Flat at zero
  on a sidecar with no `--retention-window`, which surveys and deletes nothing.

The gauges report the last completed pass, reclaiming or surveying. They are
absent until one completes, so a sidecar that has never surveyed the archive
publishes no reading rather than a reassuring zero:

- `yesnod_archive_unrecognized_keys`: objects the last pass could not classify,
  and so neither retained deliberately nor deleted. Alert on any non-zero
  value. It means the archive holds objects this version does not understand,
  that nothing will ever reclaim, and that the store grows without bound while
  every other signal looks healthy. The usual causes are a newer writer's
  objects under the same prefix and hand-placed files. Identify them before
  assuming reclamation is complete, and do not delete them by hand until you
  know what wrote them.
- `yesnod_archive_missing_objects`: objects a base the last pass retained
  names, and the store does not hold. Alert on any non-zero value, more
  urgently than on the one above: that one says the archive is too big, this
  one says it is incomplete. A retained recovery point cannot be restored, the
  condition does not heal, and no later pass repairs it -- so without this
  gauge it surfaces at a restore, which is the one moment nobody wants to
  discover it. The check costs the pass nothing, because it is a comparison
  against the listing the pass already took. The sidecar also logs one example
  key next to the count. The usual causes are an object-store lifecycle rule
  (see below), a deletion by hand, and a bucket policy or replication rule
  that never applied to every prefix.
- `yesnod_archive_wrong_length_objects`: objects a base the last pass retained
  that the store holds at a length the manifest does not claim. The other half
  of the gauge above, and the half a presence check cannot make: such an object
  fails a restore just as completely, and later -- the recovery point has
  already been chosen and everything before the object fetched. A PUT is
  atomic, so the usual cause is not the store that wrote the archive but an
  interrupted copy of it, which can simply be re-run. A length that agrees is
  not a checksum; the bytes inside an object of the right size are verified at
  a restore and nowhere cheaper.
- `yesnod_archive_damaged_bases`: bases the last pass retained that name at
  least one object that is missing or of the wrong length, and so cannot be
  restored. This is the
  gauge to page on and the one to put on a dashboard: the object count above
  says how much is gone, this says what it cost. Compare it with
  `yesnod_archive_retained_bases` -- equal means no recovery point in the
  archive is usable. The sidecar logs at error level, separately from the
  warning above, when the loss reaches the newest base, which is the one a
  restore selects by default.
- `yesnod_archive_unreadable_base_manifests`: base manifests this version
  cannot decode. Alert on any non-zero value. It is not a third kind of damage
  but a change of state: while it is non-zero the pass deletes nothing and no
  later pass will either, so the archive grows exactly as if reclamation were
  switched off. That is worth its own alert because a deletion count that has
  gone flat is also what a healthy archive with nothing to reclaim looks like.
- `yesnod_archive_retained_bases`: base images the last pass kept. Read it
  against `--retention-min-bases` and the window.
- `yesnod_archive_oldest_live_object_timestamp_seconds`: last-modified time of
  the oldest object the last pass kept. The only reading here that warns before
  the damage instead of after it, and the one to compare a bucket lifecycle rule
  against. Absent rather than zero when the pass kept nothing, because a zero
  here would read as 1970 and page immediately.
- `yesnod_archive_reclamation_last_success_timestamp_seconds`: Unix time of the
  last completed pass, and therefore how old the seven gauges above are.

A failed pass deliberately leaves the gauges at their last completed reading. A
pass that fails learns nothing, and zeroing the unrecognized count because the
pass failed would replace an absence with a false claim. Alert on staleness
instead: a freshness stamp more than one interval behind, or a rising failure
count, means the gauges no longer describe the archive.
