# Kubernetes Operator, Failover, and Certificates

## Summary

The `yesnodb.io/v1alpha1` operator manages retained local-filesystem database instances, stable role services, secure configuration, and a fail-closed asynchronous leader/follower topology. Automatic promotion first proves the old process absent, then promotes the most advanced eligible follower, while explicitly stopping short of hostile-node fencing or zero-RPO claims.

## Key Facts

- Every database instance has its own retained PVC and a `Recreate` Deployment; two Pods must never overlap on one database directory.
- Stable `-rw` and `-ro` Services select disjoint role labels, while per-instance control Services survive role changes.
- PVCs have no owner reference and survive custom-resource deletion. Generated configuration and certificate resources are cluster-owned.
- Automatic failover scales the old primary to zero and waits for no matching Pod before promotion.
- The healthiest follower with the highest visible replicated version is eligible; asynchronous replication means acknowledged writes may still be lost.
- Kubernetes process absence is not hostile-node or storage fencing. Operators must not force-delete a Pod or node whose process may still write.
- cert-manager mode uses per-instance certificates, one administrative client certificate, and a user-owned CA bundle Secret.
- Certificates are restart-rotated, not hot-reloaded. Their content participates in the Deployment identity.
- The live operator gate uses the ordinary Monty harness inside a Docker-only driver image; it does not own a second E2E binary.
- `spec.snapshot.backend: ebs` is resolved per instance from the bound PersistentVolume; the operator never accepts a caller-supplied volume ID.
- `SnapshotBackendReady` reports backup readiness without taking the serving `Ready` condition down.
- `ControlPlane::Promote` answers `CommandAccepted`, which reports that the request was **journaled**, not that it took effect. Delivery to the lifecycle loop is asynchronous, so a promotion can be lost across a restart.
- `AwaitingPromotion` returns to `Promoting` when it observes the target as a settled follower. A healthy follower has not "not promoted yet" — it has gone backwards, which is only possible if the promotion was lost.
- One metrics listener is bound for the life of the daemon process and its role-dependent half is swapped in place. A per-role listener made a role change a stop-then-rebind on the same port.

## Details

### Resource ownership and configuration

The controller renders one retained-filesystem PVC and one `Recreate` Deployment per instance. `ReadWriteOnce` is not evidence that a filesystem is safe for mmap I/O, and Kubernetes exposes no reliable field from which the controller can infer local-filesystem semantics. StorageClass selection remains an operator responsibility.

Deleting a `YesnoCluster` removes Deployments, Services, ConfigMaps, and generated certificates, but preserves data PVCs. An existing claim can be selected explicitly. Generated configuration is supplied through a Secret and hashed into the Pod template because yesnod reads TLS and configuration at startup.

Evaluation-only plaintext configuration requires `allowInsecure: true`. It is never an implicit controller default. Secure cert-manager mode and user-supplied single-instance Secret mode are mutually exclusive with that escape hatch.

### EBS snapshot backend and materialization

The operator is the independent witness that maps each claim to its PersistentVolume and EBS CSI volume handle. Deriving the volume ID from the daemon's own mount would make `verify_source_volume` tautological and unable to catch configuration pointed at another disk. The API therefore exposes backend selection and resource tags, not `volumeId`, source paths, attachment details, or local materialization. Managed Pods cannot attach or mount locally; generated EBS configuration always selects deferred materialization.

A `WaitForFirstConsumer` volume does not exist when the custom resource is first rendered. The cluster starts with portable snapshots, then one reconcile learns the bound volume, rewrites the configuration and Pod-template identity, and performs one `Recreate` rollout. `SnapshotBackendReady` makes this window visible without declaring a healthy query-serving instance unready when backup configuration is unusable. The live wait requires both the condition and running Pods carrying the Deployment's current configuration identity; status can otherwise become true before the rollout starts.

The kind gate exercises discovery through the real claim and RBAC path and must report `UnusableVolume` for its local-path volume without failing reconciliation. The AWS operator arm proves real `vol-` handles, per-instance configuration, settled rollout, follower catch-up, and retained claims. Its zero-reconciliation-error observation is one-sided and does not prove a daemon-side snapshot request reached EC2 through IRSA; that needs an explicit live lease action.

Deferred EKS materialization uses an EBS-CSI StorageClass with `encrypted: "true"` when restoring from the encrypted source snapshot. The worker's namespace-scoped token and web-identity CSI role are separate authorities. Job deletion must request background propagation: the default orphans the Pod, whose PVC-protection finalizer then pins the claim, PersistentVolume, and one EBS volume per archive.

### Retained claims outlive the cluster, deliberately, and block the next run

The controller does not owner-reference a database claim, so deleting a `YesnoCluster` leaves its PersistentVolumeClaim and the underlying EBS volume standing. That is intended -- retained storage is what the live scenario verifies after deletion -- but it means an abandoned or half-torn-down run leaves unattached volumes behind, and `terraform destroy` never knew about them because the CSI driver provisioned them, not Terraform.

The cost is not the bill. Verified on 2026-09-12: two leftover volumes were 4 GiB gp3 each, cents a month. What they do is block work, because the deferred and operator scenarios precondition on a materializer-volume count taken **by tag across a namespace that is a constant rather than per-run**. One volume orphaned by one abandoned run therefore fails every run after it, indefinitely.

Recovery is tag-based, not Terraform-based. Once a run's local state directory is gone there is nothing to destroy from, and the escape hatch's orphan sweep -- which matches available volumes by the CSI namespace tags for both the harness and operator namespaces -- is the only route. Confirm removal against the account rather than the tool's own report: query the volume ids, the account-wide volume count, and self-owned snapshots.

### Leader/follower topology and promotion

Status persists the primary instance, leadership term, promotion count, first-unavailable time, and failover stage. The state machine survives controller restarts without replaying an irreversible promotion.

Promotion has three ordered stages:

1. scale the old primary Deployment to zero and wait until no matching Pod exists;
2. select the healthy follower with the highest visible replicated version and issue promotion;
3. publish the new primary only after its control snapshot reports leader role, then render the former primary as a follower.

Duplicate queued promotion RPCs are idempotent after the first succeeds. Optional status fields must serialize `None` as JSON `null`; omitting them from a merge patch leaves stale failover state and can repeat promotion indefinitely.

The fence is limited to the process state Kubernetes can observe. Force deletion and partitions can leave the old process alive, so infrastructure-level node or storage fencing remains required. Candidate ranking exposes asynchronous progress; it does not convert replication into synchronous durability.

### A lost promotion, and the port the liveness probe watches

Two independent defects produced one symptom — a cluster stuck in `FailingOver` with `promotions=0` — and neither is visible from the other's file.

**The operator never retried.** `Promote` returns `CommandAccepted` once the request is journaled; `AwaitingPromotion` only polled. A target that restarts between accept and apply comes back a follower with no memory of the command, and nothing re-issued it. `main.rs`'s leader arm already answered a duplicate promote with "promotion already completed" *precisely* so the control API would be retry-safe across that gap — the retry the design anticipated had never been written.

The trigger for re-issuing is a **state, not a timer**. `FailoverStatus` carries no spare field, so a bounded wait needs either a CRD schema change or overloading `started_at_millis` ( which is also the promote request id ), and it buys a weaker signal that must be tuned against the slowest legitimate promotion. Observing the target as a settled follower is strictly better evidence. The `database_open` term in that predicate is load-bearing and is the one a reader would drop: mid-promotion the target has closed its follower database and not yet opened it as leader, so without it the operator re-issues on top of every promotion in progress.

**The daemon closed the port it is judged on.** A follower and a leader rendered different `/metrics` and `/readyz` from *separate listeners* bound to the same configured address, so promotion was a stop-then-rebind spanning the follower's close plus the leader's open — WAL replay and checkpoint load included. The liveness probe is `periodSeconds: 10, failureThreshold: 3`, so roughly 30 s of that window kills the container, during a promotion, which then loses it. Demotion has the identical shape.

The irony is exact: `/healthz` is a static `"ok\n"` that deliberately never consults the database, so that a database problem cannot fail liveness — and then the process closed the socket it is served on.

### cert-manager trust model

cert-manager mode creates one server certificate per instance and one administrative client certificate. Every server certificate includes its instance Service plus both stable role-Service DNS names because any instance can become leader. Instance identities authenticate as `replica`; the generated client identity authenticates as `admin` and is used by the controller for observation and promotion.

Deployments reporting Available is too early to use cert-manager. The API server additionally needs the webhook Service's endpoints registered and the CA bundle injected into its validating webhook configuration by the injector, so the first cert-manager-validated resource a harness applies can fail with a webhook call error while every Deployment is Available. It is intermittent -- four consecutive runs cleared it and a fifth did not, on a loaded host -- which is what makes it worth fixing rather than tolerating, because in CI it surfaces as an unrelated-looking failure in whatever resource happens to be applied first.

The fix is to retry that first resource for a bounded interval rather than to add another readiness wait. The retry is deliberately scoped to it: a resource whose *content* is wrong fails identically on every attempt and reports that same error once the budget expires, so retrying delays a genuine failure and hides none.

Trust comes from a separate user-owned CA Secret, not from an optional `ca.crt` in a leaf Secret. Leaf certificates and private keys are generated resources and are garbage-collected with the cluster; the CA Secret and retained data PVCs are not.

Authorization identifies a certificate by the SHA-256 fingerprint of its DER leaf. Renewal changes the principal configuration, so CA, leaves, and keys participate in the rendered Deployment identity and cause `Recreate` rolls. This is a restart-based rotation contract; hot reload remains separate work.

### Single-harness E2E

`e2e/operator/operator.py` uses the ordinary `yesno-e2e` binary and shared `op_*` verbs. Host verbs own Docker/Kubernetes operations, bounded waits, observations, and cleanup; Python owns sequencing and exact assertions. The scenario is outside the ordinary scenario directory so routine Cargo tests do not acquire a Docker dependency.

The Docker-only driver image includes the harness, operator, kind, kubectl, Docker client, and buildx plugin. It mounts the host Docker socket, joins kind's network, and uses the internal kubeconfig. The gate proves invalid-spec refusal, secure certificate issuance, retained PVCs, follower catch-up, old-Pod disappearance, exactly one promotion, former-leader rejoin, post-promotion writes, and cleanup.

The live gate found a status merge-patch defect that unit tests could not observe: skipped `None` fields preserved stale failover state and caused repeated promotions. A regression now pins the JSON `null` payload.

The AWS arm also found a follower-bootstrap defect outside the operator itself. A base image written in place exists while incomplete and becomes an unrecoverable read failure after interruption or disk exhaustion. Followers now write beside the final path, clear the matching log before rename, and rename the complete image as the single commit point; an unreadable existing image is treated as needing re-bootstrap and logged loudly.

## Files

- `yesno-operator/` - CRD types, reconciliation, rendered resources, and status state machine.
- `e2e/operator/operator.py` - live operator sequence on the common Monty runner.
- `yesno-e2e/src/operator.rs` - bounded Kubernetes host verbs.
- `scripts/gate-operator.sh` - Docker-only live gate selector.
- `yesno-operator/e2e/` - pinned driver and workload image inputs.
- `e2e/aws/operator.py` - real-EBS operator assertions on the Terraform-owned EKS cluster.
- `docs/operations.md` - operator-facing deployment, promotion, fencing, and recovery guidance.

## Test Coverage

- Unit tests pin generated insecure and mTLS configuration, retained PVC ownership, `Recreate` strategy, stable naming, status, failover stages, certificate SANs/usages, and CRD byte identity.
- A merge-patch regression requires optional status fields to clear with JSON `null`.
- The live kind scenario ingests data, catches up a follower, removes the old leader Pod, observes one promotion, writes through the stable endpoint, and verifies retained storage after deletion.
- The live AWS operator arm cross-checks configured volume IDs against bound CSI handles, waits for the rollout carrying them, bootstraps a follower, and verifies claims remain after cluster deletion.
- The harness's no-orphan-verbs audit includes the opt-in operator corpus.
- A predicate test pins that a lost promotion is recognised only from a *settled* follower, and stays silent for every in-flight shape.
- A metrics test polls `/healthz` across Starting -> Follower -> Leader -> Follower asserting zero refusals, **paired with a positive control** that performs a real stop-then-rebind of the same address and requires the poller to see failures. Without the control the first test is green whether or not the poller can detect anything.

## Pitfalls

- Do not use a rolling Deployment on one locked database directory.
- Do not put owner references on retained data PVCs.
- Do not infer local-filesystem safety from `ReadWriteOnce`.
- Do not promote until the old Pod is absent, and do not mistake that for hostile-node fencing.
- Do not claim zero RPO from asynchronous follower progress.
- Do not take trust roots from generated leaf Secrets.
- Do not omit `None` fields that must clear Kubernetes merge-patch state.
- Do not create a specialized operator E2E runner.
- Do not derive an EBS volume ID from the mount whose identity it is meant to verify.
- Do not fold snapshot-backend readiness into serving readiness.
- Do not read a configured status as proof that the matching configuration has reached running Pods.
- Do not use orphan propagation when deleting a materializer Job whose Pod holds a PVC.
- Do not read a retained claim as a leak to fix in the controller; it is intended, and the obligation it creates is a tag-based orphan sweep after any run that did not tear itself down.
- Do not judge an abandoned cloud run by its bill: a few unattached volumes cost cents and fail every subsequent run whose precondition counts them by a constant namespace tag.
- Do not read `CommandAccepted` as proof a command took effect; it reports journaling.
- Do not make `promote` synchronous to fix that — the accept-and-journal shape is deliberate; retry on the operator side instead.
- Do not bind a metrics listener per role, and do not raise `failureThreshold` to hide a window where the process closes it.
