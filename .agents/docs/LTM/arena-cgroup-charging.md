# Who pays for a touched arena page, 2026-09-30

## Question

The plugin channel hands a peer a `memfd` and `yesnod` writes lanes into it. At the
defaults that region is 512 MiB of address space per connection, sparse, so the
cost is the pages actually touched. **Whose memory limit those pages count
against decides which container an operator has to size**, and
`operator-hosted-plugin-container-plan.md` recorded an assumption rather than a
measurement: that first touch is charged, so the arena counts against `yesnod` and
not the peer.

**The assumption is wrong**, and it is wrong in the direction that causes an
OOMKill in a container nobody sized.

## Construction

Two containers from one image, sharing only a host directory for a Unix socket --
the deployment shape, and the reason it is two containers rather than two
processes is that a delegated user session cannot move a process between cgroups:
that requires write access to the common ancestor of source and destination, and
here that ancestor is root-owned.

The host creates a 256 MiB `memfd`, passes it over the socket with `SCM_RIGHTS`,
and the two sides then touch disjoint halves in three phases:

* **A** the host writes the first half
* **B** the peer *reads* that same half
* **C** the peer writes the second half, which the host never touched

**Phase C is the control and the reason A alone proves nothing.** `yesnod` both
creates the `memfd` and writes it, so an experiment where only it writes cannot
distinguish first-touch accounting from creator accounting -- both predict the
same answer.

Read from `memory.current` and `memory.stat` of each container's cgroup directly,
not from `docker stats`: `MemUsage` is a derived figure, and the question is which
*kind* of memory is charged, which only `memory.stat` answers.

## Results

```text
  256 MiB memfd, 128 MiB per half

  phase                      host                        peer
  A  host wrote 1st half     current 129.2M shmem 128.0M  current   1.1M shmem   0.0M
  B  peer read 1st half      current 129.2M shmem 128.0M  current 129.9M shmem 128.0M
  C  peer wrote 2nd half     current 129.2M shmem 128.0M  current 129.3M shmem 128.0M
```

Reproduced across three runs with hold periods of 8, 12 and 14 seconds. The first
two attempts reported the peer as `0B` and then `GONE` for the later phases,
because it exited before the driver sampled -- a sample that cannot distinguish
"zero" from "absent" is worse than no sample, and the driver now prints `GONE`
explicitly rather than a number.

## What is established

**A reader pays.** At phase B the peer has written nothing and its `shmem` is the
full 128 MiB of the half it merely read. The host's charge does not fall. **The
same pages are counted in full against both cgroups at once.**

So the operator guidance inverts: **size the arena into both containers' limits.**
A deployment that gave the peer a small limit on the theory that `yesnod` pays for
the arena would OOMKill the peer during its first large scan -- which is exactly
the failure the plan flagged as the reason to measure, arriving from the direction
the plan did not predict.

The practical bound is `maxHandles * maxBlocks * maxLanes * 8 KiB` of *touched*
pages per connection, and it applies to each side independently rather than being
split between them.

## What is not established, and is left as a question

**Phase C does not add up, and no mechanism is claimed for it.** After writing a
fresh 128 MiB that the host never touched, the peer's `shmem` is still 128.0 M --
not 256 M. It has 256 MiB of shared pages resident and mapped and is charged for
half of them. Candidate explanations, none tested:

* the first half's charge moved or was uncharged when the second half was faulted;
* `shmem` in `memory.stat` is not "resident shared pages in this cgroup" but
  something closer to a per-inode charge that does not accumulate the way the
  phase-B number suggests;
* reclaim, though no limit was set and the machine had free memory.

**The discriminating experiment a later session should run**: a peer that writes
the second half and never reads the first. If it then reports 128 M, its own
first-touch is being charged and the phase-B number must be something else; if it
reports 0 M, phase B's charge is what a mapping costs and writing adds nothing.
One arm, and it separates two readings that the three phases above cannot.

**The actionable conclusion does not wait on that.** Both sides can each report the
full touched size, which is what sizing has to assume; whether the total is
double-counted or moved between them changes the explanation and not the limit an
operator must set.

## Reproduction

`.agents-workspace/tmp/arena-charge`, `./docker-run.sh <pages> <hold-seconds>`.
Scratch, and may be deleted: this document is the finding.
