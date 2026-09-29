# Restore

This guide covers reconstructing a database from an archive, to an exact
version or a wall-clock instant, and putting a restored directory into service.
Taking the backups is in the [backup guide](backup.md).

## Restoring an archive

Restore an archive into a directory that does not exist:

```console
yesnoctl restore \
  --store s3://yesno-backups/production \
  --target /var/lib/yesno-restored \
  --target-version 1842
```

The target is an exact committed logical version, not an LSN. The restore
selects a base at or below it, validates object sizes and CRCs, verifies the
unbroken per-shard history, rejects gaps and forks, truncates to a globally
complete multi-shard transaction prefix, and opens the staging directory with
ordinary replica recovery before publishing it with one rename. Omit
`--target-version` to restore the durable archive tip. A requested version that
is not present as a complete commit fails rather than rounding silently.

## Point-in-time recovery

A target may instead be a wall clock:

```console
yesnoctl restore \
  --store s3://yesno-backups/production \
  --target /var/lib/yesno-restored \
  --target-time 2026-09-06T14:02:00Z
```

`--target-time` and `--target-version` are mutually exclusive. The instant must
be RFC 3339 and must carry an offset; a bare local time is refused rather than
assumed to be UTC, because guessing wrong shifts the recovery point by hours
without saying so.

Every commit records the wall-clock time its version was assigned, taken at the
moment the version is assigned and constrained to be non-decreasing in version
order. A backward system-clock step is absorbed rather than allowed to reorder
two commits, so recorded times can run slightly ahead of true time for the
duration of such a step. Object upload timestamps are still never used as a
proxy: an upload may be retried or delayed long after the commit it carries.

The target resolves to the highest committed version at or before the instant,
and the restore then proceeds exactly as for a version target. By default a
commit stamped exactly at the target is included; `--target-exclusive` drops it,
which is what to use when the instant you have is the start of the transaction
you are recovering away from.

A wall-clock target over history written before commit-time stamping existed
fails and names the first version it cannot place. It does not round, and it
does not treat an unrecorded time as the epoch. Recover that history by version
instead.

Ask which targets an archive can answer before choosing one:

```console
yesnoctl restore --store s3://yesno-backups/production --inspect
```

This reads manifests and object descriptors only. It downloads no data, writes
nothing, creates no directory, needs no `--target`, and is safe to run against a
live archive. Each line reports one recovery window -- its leadership term, base
generation, the version and time range it can restore to, whether that whole
window answers wall-clock targets, and whether it is the archive's active root.

## After the restore, choose a timeline

`--target-action` decides what happens to the recovered directory:

| value | effect |
|---|---|
| `publish` | rename it onto the target. The default. |
| `pause` | verify and recover it, then stop, leaving it unpublished under a temporary sibling name that the completion line reports. For confirming a target produced what you expected before committing to it. |
| `promote` | raise the leadership term before publishing, so the copy is a new timeline. |

Use `promote` whenever the restored database will be written to, or archived, or
replicated from. A restored copy otherwise carries the original's term, and it
shares the original's database UUID -- so nothing downstream can tell the two
histories apart, which is exactly the case a UUID cannot fence. A restore taken
only to read a past state does not need it.

A paused directory is left in place deliberately and is not cleaned up. Remove
it yourself once you are finished with it.

## Putting a restored directory into service

The following steps apply after either `yesnoctl basebackup` produced the
directory or `yesnoctl restore` reconstructed it from an archive.

1. Stop `yesnod` and confirm no process has the target directory open.
2. Put one completed backup directory at the configured local data path. Do not
   merge its files with another backup or an existing database.
3. Keep the restored files owned by the service account.
4. Start `yesnod` against that directory.
5. Run `yesno status` and sample known keys.
6. Run `yesnoctl checkpoint` once and take a fresh [backup](backup.md) after
   validation.

Do not merge files from different backups or database identities.
