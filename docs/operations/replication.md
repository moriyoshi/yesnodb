# Replication and disaster recovery

This guide covers asynchronous replication to standbys, measuring recovery
objectives, promoting a standby after a leader failure, and the drills to run
before relying on any of it. Replication is not a backup; see the
[backup guide](backup.md).

## Leader and standby configuration

A leader exposes lifecycle control, events, and physical replication on one
shared gRPC listener. Bootstrap transfers complete database images, so grant
the `replication` capability only to explicit principals and source networks:

```toml
[server.control]
listen = "10.0.0.10:50052"
journal_dir = "/var/lib/yesno-control"

[server.control.tls]
cert = "/etc/yesno/tls/server.pem"
key = "/etc/yesno/tls/server.key"
client_ca = "/etc/yesno/tls/replicas-ca.pem"
require_client_auth = true

[[auth.principal]]
name = "standby-b"
role = "replica"
cert_sha256 = "lowercase-sha256-of-standby-certificate-der"

[[auth.rule]]
principal = "standby-b"
address = "10.0.0.0/24"
capability = "replication"
action = "allow"
```

A standby configuration names the leader's shared control endpoint:

```toml
[server]
role = "follower"
data_dir = "/var/lib/yesno"

[follower]
leader = "https://leader.internal:50052"
poll_interval_secs = 1
max_backoff_secs = 30
serve_reads = true

[follower.tls]
ca = "/etc/yesno/tls/replicas-ca.pem"
cert = "/etc/yesno/tls/standby-b.pem"
key = "/etc/yesno/tls/standby-b.key"
domain = "leader.internal"

[server.flight]
listen = "127.0.0.1:50051"

[server.metrics]
listen = "127.0.0.1:9750"
```

Read-serving followers are always potentially stale. Do not use them for
read-after-write.

## Recovery objectives

A deployment has one leader and any number of standbys, with asynchronous
replication and no consensus or automatic election. A commit is durable once
the leader has synchronized its WAL, before a standby necessarily has it. RPO
is therefore greater than zero by construction.

Measure that exposure under the intended workload. Compare the leader's
`yesnod_visible_version` with each standby's
`yesnod_follower_visible_version`, and time how long an acknowledged probe
commit takes to appear there. `yesnod_follower_connected`,
`yesnod_follower_halted`, and `yesnod_follower_catchup_failures_total` expose
replication health, but the configured poll interval alone is not an RPO
measurement. Publish the observed window. RTO includes detecting and
externally fencing the failed leader, selecting a standby, promoting it, and
repointing clients. Promotion itself is only one part of that interval.

## Disaster recovery: promotion and fencing

Before promotion, fence the old leader by a mechanism outside yesnodb: stop its
service, revoke its virtual address, or otherwise prove it cannot accept
writes. yesnodb detects superseded terms at followers and clients; it does not
prevent two reachable leaders.

Poll every standby and record its `visible_version` before changing any roles:

```console
for host in standby-b standby-c standby-d; do
  printf '%s ' "$host"
  yesno --endpoint "https://$host:50051" status | grep visible
done
```

Choose the standby with the highest complete `visible_version`, not the most
bytes applied or the highest single-shard LSN. The visible version is the
cross-shard watermark, so it names the newest transactionally complete state.
Promote that standby with:

```console
systemctl kill -s SIGUSR1 yesnod
```

Promotion performs a final catch-up attempt, raises and persists the leadership
term, recovers a consistent cross-shard prefix, and opens the local database as
leader. Recovery truncates each shard at its first unresolved record rather
than retaining half of a multi-shard commit. Repoint clients and pin the new
term on requests:

```console
yesno --endpoint https://new-leader:50051 status
yesno --endpoint https://new-leader:50051 --min-term NEW_TERM stats
```

Repoint the remaining standbys to the new leader and distinguish these outcomes:

- A standby that resumes following needs no repair.
- A halt reason saying the endpoint offers an older leadership term means the
  standby is pointed at a superseded leader. Check the address before doing
  anything destructive.
- `out_of_range` with the follower ahead of the leader means the standby holds
  commits missing from the promoted node. This is a reportable data-loss event.
  Preserve that standby as evidence and record the lost version range before
  rebuilding it.
- `failed_precondition` indicating that retained WAL no longer reaches the
  follower means it is behind and must bootstrap again. Do not confuse this
  ordinary retention gap with an ahead-of-leader data-loss report.
- A warning that a base image is unusable, followed by that shard being
  bootstrapped again, needs no action. A standby whose copy of a shard cannot
  be read replaces it rather than stopping, so an interrupted bootstrap or a
  damaged image costs one re-copy of that shard and no intervention. Repeated
  occurrences for the same shard are worth investigating as storage faults.

Never rejoin the old leader incrementally. Stop it, preserve any evidence needed
for incident review, empty its old data directory, and bootstrap it as a new
standby. Its database identity matches the new leader but its timeline does not.

After repointing, run `yesno status` and `yesno stats` against the new leader,
compare a sample of known keys with the latest backup, and run `yesnoctl checkpoint`
against its control endpoint. The reported watermark must be at
least the recorded `visible_version` of the promoted standby.

## Drill and acceptance criteria

Before relying on replication, rehearse:

- leader bootstrap and standby catch-up;
- stale reads on a read-serving follower;
- old-leader fencing;
- manual promotion;
- client `--min-term` refusal against the old leader;
- backup restore into an empty directory;
- a standby rebuild after falling behind retained WAL.

Record measured replication lag, backup duration, restore duration, and
promotion time under the intended workload. Those measurements, not the
configured poll interval, define the deployment's actual RPO and RTO.
