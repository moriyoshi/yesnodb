# The host side of the real-AWS gate, and the only entry point to it.
#
# Runs only through scripts/gate-aws.sh, which sets YESNO_AWS_GATE=1; without
# it every cloud_* verb refuses, because this file creates billable resources.
#
# Until 2026-09-02 this sequence was a Terraform local-exec graph and two shell
# scripts, and the runner script was a .tftpl delivered through cloud-init.
# All three reported one boolean between them: `terraform apply` succeeded.
# It is a scenario now for the reason every other integration here is one --
# the sequence, the runner script and the assertions belong in Python, the host
# tools belong behind narrow verbs, and a gate costing twenty billable minutes
# per attempt has to say *which* step failed the first time it fails.
#
# The behavioural oracles are e2e/aws/ebs.py, e2e/aws/deferred_ecs.py and
# e2e/aws/deferred_eks.py -- local materialization and both deferred
# materializers -- running one after another on the instance this file
# provisions. What is asserted here is the delivery of them.

SOURCE_MOUNT = "/mnt/yesno-source"
SNAPSHOT_MOUNT = "/mnt/yesno-snapshots"
# The slim runner image, deliberately not the all-in-one yesno-e2e:local one:
# this is pushed to a per-run ECR repository and pulled onto a fresh EC2 host,
# so its size is transfer cost on every billable run rather than a cache.
RUNNER_DOCKERFILE = "yesno-operator/e2e/aws.Dockerfile"
# Three tags out of one Dockerfile and one build. The `runner` stage has no
# ENTRYPOINT -- ECS runs a different binary out of the same image, and an
# ENTRYPOINT would prepend the harness to it -- so every invocation names what
# it wants. The other two exist because a `YesnoCluster` does the opposite: it
# names an image and supplies `args` alone, exactly as a user's manifest does,
# so that image has to start its own process. They are one metadata layer over
# `runner`, so they cost a build of nothing and a push of nothing.
RUNNER_TARGET = "runner"
YESNOD_TARGET = "yesnod"
OPERATOR_TARGET = "operator"
HARNESS = "yesno-e2e"
CLEANUP_COMMAND = "yesno-aws-cleanup"
EBS_SCENARIO = "e2e/aws/ebs.py"
EBS_SECONDS = 1200
ECS_SCENARIO = "e2e/aws/deferred_ecs.py"
# Longer than the local arm: two Fargate tasks have to start, and a task that
# restores an EBS snapshot is minutes of that before it runs anything.
ECS_SECONDS = 3000
EKS_SCENARIO = "e2e/aws/deferred_eks.py"
# Longer again. A Kubernetes Job has to be scheduled, a VolumeSnapshot has to
# become ready, and the EBS CSI driver has to provision and attach a volume
# from it -- three waits the Fargate arm does in one.
EKS_SECONDS = 4500
OPERATOR_SCENARIO = "e2e/aws/operator.py"
# The operator installs a CRD, cert-manager and itself, then waits for two
# database instances whose claims are real EBS volumes the CSI driver has to
# create and attach -- and for one restart, because the configuration changes
# the moment those volumes become known.
OPERATOR_SECONDS = 2400
# The installer account the bootstrap creates in `kube-system` and the operator
# arm authenticates as. Named here because nothing in the stack refers to it.
OPERATOR_INSTALLER = "yesno-operator-installer"

# Everything below the shell assignments is fixed text. Keeping the varying
# part to a prologue of `name=value` lines is what lets the body read as the
# shell script it is, with no interpolation anywhere near the awk braces.
#
# POSIX sh, not bash: Systems Manager runs AWS-RunShellScript through /bin/sh,
# and the arrays the cloud-init template used were only safe because that
# version was a file with a bash shebang.
PROVISION_BODY = """
# cloud-init still holds the package database on a freshly booted instance.
cloud-init status --wait
dnf install -y amazon-ecr-credential-helper docker nfs-utils nvme-cli
systemctl enable --now docker
install -d -m 0700 /root/.docker
printf '%s\\n' '{"credsStore":"ecr-login"}' >/root/.docker/config.json

# Nitro renames the attachment, so the volume id -- exposed as the NVMe serial
# with its hyphen removed -- is the only stable handle back to the device
# Terraform attached.
device=""
for candidate in \\
    "/dev/disk/by-id/nvme-Amazon_Elastic_Block_Store_$token" \\
    /dev/xvdf /dev/sdf; do
    if [ -b "$candidate" ]; then
        device="$(readlink -f "$candidate")"
        break
    fi
done
if [ -z "$device" ]; then
    device="$(lsblk -ndo PATH,SERIAL | awk -v serial="$token" '$2 == serial { print $1; exit }')"
fi
if [ -z "$device" ] || [ ! -b "$device" ]; then
    echo "cannot resolve source EBS device $volume_id" >&2
    lsblk -o NAME,PATH,SERIAL,SIZE,TYPE >&2
    exit 1
fi

# A first run formats the whole volume; a retried one must not.
if ! blkid -s TYPE -o value "$device" >/dev/null 2>&1; then
    mkfs.ext4 -q "$device"
fi

# The privileged agent mounts lease clones inside its own container and the
# daemon has to see them, so /mnt must be a shared mount before either starts,
# and binding it to itself is what gives it a mount entry to mark.
#
# This has to happen before anything is mounted *underneath* it, which is
# why it is here rather than next to the mounts it exists for. `mount --bind`
# is not recursive: a bind taken after the volumes were mounted overlays /mnt
# with the empty directories on the root filesystem and shadows both of them.
# That failure does not look like a mount problem downstream -- findmnt
# reports the source mount as `/dev/<root device>[/mnt/yesno-source]`, and
# yesnod refuses to snapshot a device that is not the volume it was configured
# with. `mount --rbind` would carry existing submounts across, but it leaves
# two mount entries per target and `findmnt --target` then answers with two
# lines, which breaks the same check a second way.
#
# `--make-rprivate` before `--make-rshared`, and neither is optional.
# systemd runs with `MountFlags=shared`, so `/` on this host is already shared,
# and binding a directory out of a shared mount puts the new mount in *the same
# peer group as its source*. Everything mounted under /mnt would then propagate
# back into `/` at the same path, giving two mount entries per filesystem --
# `findmnt --target` answers with two identical lines, `umount` leaves the copy
# standing, and the daemon's source check fails on the pair. Making the bind
# private detaches it from `/`; making it shared again gives it a peer group of
# its own, which is the one the agent and the daemon share.
if ! mountpoint -q /mnt; then
    mount --bind /mnt /mnt
fi
mount --make-rprivate /mnt
mount --make-rshared /mnt

mkdir -p "$source_mount" "$snapshot_mount" "$staging_mount"
if ! mountpoint -q "$source_mount"; then
    mount "$device" "$source_mount"
fi

# The staging filesystem the deferred arm shares with its Fargate worker. Plain
# NFS 4.1 rather than amazon-efs-utils: the mount target's DNS name is all that
# is needed here, and efs-utils would add a stunnel process to the host for a
# filesystem that only carries a copy of a snapshot.
if ! mountpoint -q "$staging_mount"; then
    attempt=0
    until mount -t nfs4 \\
        -o nfsvers=4.1,rsize=1048576,wsize=1048576,hard,timeo=600,retrans=2,noresvport \\
        "$efs_id.efs.$region.amazonaws.com:/" "$staging_mount"; do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 30 ]; then
            echo "cannot mount EFS $efs_id at $staging_mount" >&2
            exit 1
        fi
        sleep 10
    done
fi

# The mount topology the daemon's own source check depends on, verified where
# it is cheap. Without this the first symptom is yesnod refusing to snapshot,
# eight minutes and one container start later, naming a device nobody
# configured. findmnt has to answer with exactly one line naming the volume:
# two lines mean the mount was replicated, and a `device[/subpath]` answer
# means /mnt was bound over its own submounts and the source mount being
# reported is an empty directory on the root filesystem.
resolved="$(findmnt --noheadings --output SOURCE --target "$source_mount")"
if [ "$(printf '%s\\n' "$resolved" | wc -l)" -ne 1 ] || [ "$resolved" != "$device" ]; then
    echo "source mount $source_mount resolves to '$resolved', not '$device'" >&2
    findmnt --output TARGET,SOURCE,PROPAGATION >&2
    exit 1
fi

# Shared propagation is the other half, and it fails separately: a submount
# stranded outside the /mnt bind keeps its filesystem visible but stays
# private, and `docker run --mount bind-propagation=rshared` then refuses it.
for checked in /mnt "$source_mount"; do
    propagation="$(findmnt --noheadings --output PROPAGATION --target "$checked")"
    case "$propagation" in
        shared) ;;
        *)
            echo "mount $checked is '$propagation', not shared" >&2
            findmnt --output TARGET,SOURCE,PROPAGATION >&2
            exit 1
            ;;
    esac
done

printf 'yesno-device=%s\\n' "$device"
"""

EBS_BODY = """
docker pull "$image"

# The scenario proves ordinary cleanup; this is the failure path. A lease
# outliving a failed assertion is a chargeable snapshot and a chargeable clone,
# and Terraform cannot destroy what it never created. Unmount first: the daemon
# is gone by now and only the agent's mounts stand between the clone and its
# deletion.
cleanup() {
    # Retention is only useful if it reaches this far. Keeping the Terraform
    # stack while this trap still tears down the lease's mounts leaves a run that
    # is billing and has nothing left to look at.
    if [ "$keep" = "1" ]; then
        echo "gate-aws: YESNO_AWS_KEEP=1; leaving the lease mounts and the snapshot and volume in place"
        return
    fi
    findmnt --raw --noheadings --output TARGET --submounts "$snapshot_mount" 2>/dev/null |
        sort --reverse |
        while read -r target; do
            if [ "$target" != "$snapshot_mount" ]; then
                umount "$target" || true
            fi
        done || true
    docker run $common "$image" "$cleanup_command" || true
}
trap cleanup EXIT

# --privileged, /dev and the two rshared binds are the agent's, not the
# daemon's: the daemon runs unprivileged inside this same container and the
# scenario on the far side asserts that it does.
docker run $common \\
    --privileged \\
    --mount type=bind,source=/dev,target=/dev \\
    --mount type=bind,source=/sys,target=/sys,readonly \\
    --mount "type=bind,source=$source_mount,target=$source_mount,bind-propagation=rshared" \\
    --mount "type=bind,source=$snapshot_mount,target=$snapshot_mount,bind-propagation=rshared" \\
    "$image" \\
    "$harness" --timeout "$timeout" "$scenario"
"""

ECS_BODY = """
# No --privileged, no /dev, no propagation flags, and no snapshot mount. The
# deferred arm mounts nothing on this host, and the container it runs in is the
# statement of that: a scenario that quietly needed a mount would fail here
# rather than borrow one.
cleanup() {
    # Retention is only useful if it reaches this far. Keeping the Terraform
    # stack while this trap still tears down the Fargate task leaves a run that
    # is billing and has nothing left to look at.
    if [ "$keep" = "1" ]; then
        echo "gate-aws: YESNO_AWS_KEEP=1; leaving the Fargate task and its restored volume in place"
        return
    fi
    # A worker outliving an interrupted run is worse than a leaked snapshot:
    # `terraform destroy` cannot delete a cluster that still has an active
    # task, so the whole stack would stay standing. The runner's role may stop
    # tasks in this cluster and nothing else.
    for task in $(aws ecs list-tasks --cluster "$cluster" --desired-status RUNNING \\
        --query 'taskArns[]' --output text 2>/dev/null); do
        aws ecs stop-task --cluster "$cluster" --task "$task" \\
            --reason "yesno deferred gate cleanup" >/dev/null 2>&1 || true
    done
    docker run $common "$image" "$cleanup_command" || true
}
trap cleanup EXIT

docker run $common \\
    --mount "type=bind,source=$source_mount,target=$source_mount" \\
    --mount "type=bind,source=$staging_mount,target=$staging_mount" \\
    "$image" \\
    "$harness" --timeout "$timeout" "$scenario"
"""


# Everything the archiver needs *inside* the cluster, created with curl against
# the Kubernetes API.
#
# There is no kubectl here and no Kubernetes Terraform provider. kubectl
# would be a downloaded binary on the host; the provider would need a working
# cluster client at plan time to create objects whose CRDs the same apply is
# still installing. The runner's instance role is a cluster administrator
# through an EKS access entry, `aws eks get-token` turns that into a bearer
# token, and the API server takes JSON over HTTPS like anything else.
#
# The archiver's own credential is a namespace-scoped ServiceAccount token
# minted by the TokenRequest API and written to a file on the instance. It is
# never a Terraform output and never travels through Systems Manager, where a
# command document keeps its parameters.
EKS_BOOTSTRAP_BODY = """
# Every curl below is bounded, and that is what makes the retry loop's
# "60 attempts, five seconds apart" the five-minute ceiling it looks like.
# curl has no default overall timeout, so against an endpoint that accepts no
# connection each attempt costs a multi-minute connect instead of failing fast,
# and the loop then runs until Systems Manager kills the command. A bootstrap
# with a nominal ceiling of five minutes was still going after ten on
# 2026-09-02. A retry loop whose per-attempt cost is unbounded has no bound.
curl_timeouts="--connect-timeout 10 --max-time 60"
install -d -m 0755 "$kube_dir"
ca_file="$kube_dir/ca.crt"
out="$kube_dir/response.json"
printf '%s' "$ca_data" | base64 -d >"$ca_file"

admin="$(aws eks get-token --region "$region" --cluster-name "$cluster" \
    --query status.token --output text)"
if [ -z "$admin" ]; then
    echo "aws eks get-token returned nothing for cluster $cluster" >&2
    exit 1
fi

# POST one object, tolerating the 409 a retried run produces. Anything else is
# reported with the API server's own message, which is the only place a
# rejected manifest explains itself.
apply() {
    echo "gate-aws: POST $1"
    code="$(curl -sS $curl_timeouts -o "$out" -w '%{http_code}' --cacert "$ca_file" \
        -H "Authorization: Bearer $admin" \
        -H 'Content-Type: application/json' \
        -X POST --data-binary @- "$endpoint$1")"
    case "$code" in
        20*|409) return 0 ;;
    esac
    echo "kubernetes POST $1 returned $code" >&2
    cat "$out" >&2
    return 1
}

# The snapshot CRDs arrive with the snapshot-controller add-on, and the API
# server registers them a moment after the add-on reports itself active. The
# archiver's whole pre-provisioned path is written in terms of them, so this is
# a boundary rather than a courtesy sleep.
attempt=0
until curl -sS $curl_timeouts -o /dev/null --fail --cacert "$ca_file" \
    -H "Authorization: Bearer $admin" \
    "$endpoint/apis/snapshot.storage.k8s.io/v1"; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 60 ]; then
        echo "the snapshot.storage.k8s.io API never appeared" >&2
        exit 1
    fi
    sleep 5
done

apply /api/v1/namespaces <<JSON
{"apiVersion":"v1","kind":"Namespace","metadata":{"name":"$namespace"}}
JSON

apply "/api/v1/namespaces/$namespace/serviceaccounts" <<JSON
{"apiVersion":"v1","kind":"ServiceAccount",
 "metadata":{"name":"$account","namespace":"$namespace"}}
JSON

# Exactly what yesno-archive does and nothing else: create, read and delete a
# cluster-scoped VolumeSnapshotContent, and the same three verbs on the
# VolumeSnapshot, the claim and the Job inside one namespace. A cluster-admin
# token would have proved the materializer works and said nothing about what
# it needs.
apply /apis/rbac.authorization.k8s.io/v1/clusterroles <<JSON
{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole",
 "metadata":{"name":"$account"},
 "rules":[{"apiGroups":["snapshot.storage.k8s.io"],
           "resources":["volumesnapshotcontents"],
           "verbs":["create","get","delete"]}]}
JSON

apply /apis/rbac.authorization.k8s.io/v1/clusterrolebindings <<JSON
{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding",
 "metadata":{"name":"$account"},
 "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"$account"},
 "subjects":[{"kind":"ServiceAccount","name":"$account","namespace":"$namespace"}]}
JSON

apply "/apis/rbac.authorization.k8s.io/v1/namespaces/$namespace/roles" <<JSON
{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role",
 "metadata":{"name":"$account","namespace":"$namespace"},
 "rules":[{"apiGroups":["snapshot.storage.k8s.io"],
           "resources":["volumesnapshots"],
           "verbs":["create","get","delete"]},
          {"apiGroups":[""],
           "resources":["persistentvolumeclaims"],
           "verbs":["create","get","delete"]},
          {"apiGroups":["batch"],
           "resources":["jobs"],
           "verbs":["create","get","delete"]}]}
JSON

apply "/apis/rbac.authorization.k8s.io/v1/namespaces/$namespace/rolebindings" <<JSON
{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding",
 "metadata":{"name":"$account","namespace":"$namespace"},
 "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"$account"},
 "subjects":[{"kind":"ServiceAccount","name":"$account","namespace":"$namespace"}]}
JSON

apply /apis/snapshot.storage.k8s.io/v1/volumesnapshotclasses <<JSON
{"apiVersion":"snapshot.storage.k8s.io/v1","kind":"VolumeSnapshotClass",
 "metadata":{"name":"$snapshot_class"},
 "driver":"ebs.csi.aws.com","deletionPolicy":"Delete"}
JSON

# WaitForFirstConsumer, because the claim is bound before the Job is scheduled
# and a volume provisioned into the wrong Availability Zone cannot be attached.
#
# `encrypted: "true"` is not optional here, and its absence does not fail
# where you would look. The gate's source volume is encrypted, so its snapshot
# is; the EBS CSI driver passes `Encrypted` to `CreateVolume` explicitly from
# this parameter and defaults it to **false**, and EC2 then refuses:
#
#   api error InvalidParameterCombination: EncryptedVolume parameter [false]
#   is inconsistent with snapshot encryption state [true]
#
# Nothing upstream notices. The claim stays Pending, the Job's pod is never
# scheduled, and the archiver reports a Job that timed out -- three layers away
# from a StorageClass missing one field. That is a live failure from 2026-09-04.
#
# No `kmsKeyId`: the source volume uses the account's default EBS key, and
# omitting it here restores under the same one.
apply /apis/storage.k8s.io/v1/storageclasses <<JSON
{"apiVersion":"storage.k8s.io/v1","kind":"StorageClass",
 "metadata":{"name":"$storage_class"},
 "provisioner":"ebs.csi.aws.com","parameters":{"type":"gp3","encrypted":"true"},
 "volumeBindingMode":"WaitForFirstConsumer","reclaimPolicy":"Delete"}
JSON

# The shared RWX staging claim, statically bound to the same EFS filesystem the
# runner has mounted and the ECS arm uses. Retain, because Terraform owns the
# filesystem and a reclaim policy must not delete it.
apply /api/v1/persistentvolumes <<JSON
{"apiVersion":"v1","kind":"PersistentVolume","metadata":{"name":"$staging_claim"},
 "spec":{"capacity":{"storage":"8Gi"},"volumeMode":"Filesystem",
 "accessModes":["ReadWriteMany"],"persistentVolumeReclaimPolicy":"Retain",
 "storageClassName":"","csi":{"driver":"efs.csi.aws.com","volumeHandle":"$efs_id"}}}
JSON

apply "/api/v1/namespaces/$namespace/persistentvolumeclaims" <<JSON
{"apiVersion":"v1","kind":"PersistentVolumeClaim",
 "metadata":{"name":"$staging_claim","namespace":"$namespace"},
 "spec":{"accessModes":["ReadWriteMany"],"storageClassName":"",
 "volumeName":"$staging_claim","resources":{"requests":{"storage":"8Gi"}}}}
JSON

# A bounded ServiceAccount token, rather than a Secret the token controller
# has to populate. One request, no waiting, and it expires with the day.
#
# One function called twice, because this run needs two credentials and they
# must not become two implementations. The archiver's is namespace-scoped by
# design; the operator arm's is administrative by necessity. Which account is
# asked for is the only thing that differs.
mint_kubeconfig() {
    mint_namespace="$1"
    mint_account="$2"
    mint_out="$3"

    printf '%s' \
        '{"apiVersion":"authentication.k8s.io/v1","kind":"TokenRequest",' \
        '"spec":{"expirationSeconds":86400}}' >"$kube_dir/tokenrequest.json"
    code="$(curl -sS $curl_timeouts -o "$out" -w '%{http_code}' --cacert "$ca_file" \
        -H "Authorization: Bearer $admin" \
        -H 'Content-Type: application/json' \
        -X POST --data-binary "@$kube_dir/tokenrequest.json" \
        "$endpoint/api/v1/namespaces/$mint_namespace/serviceaccounts/$mint_account/token")"
    case "$code" in
        20*) ;;
        *)
            echo "kubernetes TokenRequest for $mint_namespace/$mint_account returned $code" >&2
            cat "$out" >&2
            return 1
            ;;
    esac
    # Joined onto one line first, and whitespace tolerated around the colon.
    # sed is line-based and the pattern wants `"token":"..."` intact; a
    # pretty-printed reply puts a newline before the key and a space after the
    # colon, and the pattern that assumed compact JSON then produced an empty
    # token and a message about the wrong thing. Verified against both shapes.
    token="$(tr -d '\\n\\r' <"$out" | sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\\([^"]*\\)".*/\\1/p')"
    if [ -z "$token" ]; then
        echo "kubernetes TokenRequest for $mint_namespace/$mint_account returned nothing" >&2
        # Redacted by *shape*, not by key. This design keeps the
        # ServiceAccount token out of Systems Manager, whose command output is
        # retrievable -- and a redaction keyed on `"token":` would fail on
        # exactly the replies this dump exists to explain, since those are the
        # ones the key-based pattern above has already failed to read.
        sed 's/ey[A-Za-z0-9_-]\\{16,\\}[^"]*/<redacted>/g' "$out" >&2
        return 1
    fi

    umask 077
    printf '%s\n' \
        'apiVersion: v1' \
        'kind: Config' \
        'clusters:' \
        '- name: yesno' \
        '  cluster:' \
        "    server: $endpoint" \
        "    certificate-authority-data: $ca_data" \
        'users:' \
        '- name: yesno' \
        '  user:' \
        "    token: $token" \
        'contexts:' \
        '- name: yesno' \
        '  context:' \
        '    cluster: yesno' \
        '    user: yesno' \
        'current-context: yesno' \
        >"$mint_out"
    chmod 0600 "$mint_out"
    rm -f "$out" "$kube_dir/tokenrequest.json"
}

mint_kubeconfig "$namespace" "$account" "$kubeconfig"

# The operator arm's installer identity.
#
# Bound to the built-in `cluster-admin`, and deliberately rather than by
# omission: installing a CustomResourceDefinition and a ClusterRole is an
# administrative act, and it is exactly what `kubectl apply -f deploy/` is for a
# user. It is **not** the operator's own authority. That comes from the
# ClusterRole in the checked-in manifest, which this arm installs and the
# controller then runs under -- a different subject, and the only one under
# test. Scoping the installer finely would describe an installer nobody has and
# would test the harness rather than the product.
apply /api/v1/namespaces/kube-system/serviceaccounts <<JSON
{"apiVersion":"v1","kind":"ServiceAccount",
 "metadata":{"name":"$installer","namespace":"kube-system"}}
JSON

apply /apis/rbac.authorization.k8s.io/v1/clusterrolebindings <<JSON
{"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding",
 "metadata":{"name":"$installer"},
 "roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"cluster-admin"},
 "subjects":[{"kind":"ServiceAccount","name":"$installer","namespace":"kube-system"}]}
JSON

mint_kubeconfig kube-system "$installer" "$operator_kubeconfig"

printf 'yesno-kubeconfig=%s\n' "$kubeconfig"
printf 'yesno-operator-kubeconfig=%s\n' "$operator_kubeconfig"
"""

EKS_BODY = """
# Defined again here, because this is a different script from the bootstrap
# and `set -u` would abort the whole arm on the unset variable. The cleanup
# below runs from a trap, where an unbounded curl does not merely stall: it
# holds up `terraform destroy` behind it.
curl_timeouts="--connect-timeout 10 --max-time 60"

# Why the arm failed, in the cluster's own words.
#
# Called from the trap *before* anything is deleted, which is the only moment
# this evidence exists. The archiver removes its Job and claim as soon as
# materialization fails, and the namespace delete below takes the events with
# it. Run any later -- which is where this started, after the scenario returned
# -- and it finds an empty namespace and reports that as though the cluster had
# nothing to say.
#
# Systems Manager truncates a command's output at 24 KB, so each response is
# reduced to the keys that ever carry a reason, and capped. Not jq: the runner
# does not have it, and a diagnostic that needs its own install is one that will
# not run when it is wanted.
dump_cluster() {
    api() {
        curl -sS $curl_timeouts --cacert "$kube_dir/ca.crt" -H "Authorization: Bearer $token" "$endpoint$1"
    }
    # Whole key/value pairs, matched in place. The first version split on
    # commas and grepped -- which silently destroyed exactly the messages it
    # existed to surface: a live `ProvisioningFailed` on 2026-09-03 read
    # "... operation error EC2: CreateVolume" and stopped, because everything
    # after the message's first comma became a fragment with no key and was
    # dropped. The AWS error was in those fragments.
    #
    # The alternation handles `\\"` inside a message, and the whitespace after
    # the colon is not optional decoration: this API server pretty-prints.
    #
    # The cap is per section and the budget is shared, because Systems
    # Manager truncates the whole command at 24 KB. Sections that carry answers
    # get more than the ones that carry context.
    show() {
        echo "=== $1 ==="
        api "$2" | grep -oE '"(name|message|reason|readyToUse|phase|type|status|state|exitCode)":[[:space:]]*("(\\\\.|[^"\\\\])*"|[A-Za-z0-9]+)' | head -c "${3:-1200}" || true
        echo
    }
    # Warnings first and with the largest budget. Normal events -- every
    # SnapshotReady, WaitForFirstConsumer and SuccessfulCreate -- outnumber them
    # and crowded the answer off the end once already. A `ProvisioningFailed`
    # carries the provisioner's own words and is the whole reason to look.
    show "Events (warnings)" "/api/v1/namespaces/$namespace/events?fieldSelector=type%21%3DNormal" 6000

    # The provisioner itself, which lives in kube-system and so appears in
    # none of the sections below. A claim stuck in ExternalProvisioning says the
    # controller did not act; only the controller says why not.
    show "EBS CSI controller" "/api/v1/namespaces/kube-system/pods?labelSelector=app%3Debs-csi-controller" 1500

    show "VolumeSnapshots" "/apis/snapshot.storage.k8s.io/v1/namespaces/$namespace/volumesnapshots"
    show "VolumeSnapshotContents" "/apis/snapshot.storage.k8s.io/v1/volumesnapshotcontents"
    show "PersistentVolumeClaims" "/api/v1/namespaces/$namespace/persistentvolumeclaims"
    show "Jobs" "/apis/batch/v1/namespaces/$namespace/jobs"
    show "Pods" "/api/v1/namespaces/$namespace/pods"
    # Events outlive the objects they describe, so this is the section that
    # still answers "why did the pod never start" after the archiver has already
    # deleted its Job and claim.
    show "Events" "/api/v1/namespaces/$namespace/events"
    for pod in $(api "/api/v1/namespaces/$namespace/pods" | tr ',' '\\n' | sed -n 's/.*"name":"\\(yesno-[a-z0-9-]*\\)".*/\\1/p' | sort -u); do
        echo "=== log: $pod ==="
        api "/api/v1/namespaces/$namespace/pods/$pod/log?tailLines=40" 2>&1 | head -c 1500 || true
        echo
    done
}

# The kubeconfig is bind-mounted read-only rather than passed as a variable:
# a bearer token in `docker run -e` is visible in every process listing on the
# host, and in the Systems Manager command document that shipped this script.
cleanup() {
    status=$?
    token="$(aws eks get-token --region "$region" --cluster-name "$cluster" --query status.token --output text 2>/dev/null || true)"

    # Before anything is deleted, and only when the arm failed: on a green
    # run this would add several kilobytes to every gate run for nothing.
    if [ -n "$token" ] && [ "$status" != "0" ]; then
        dump_cluster
    fi

    # Retention has to reach in here. Keeping the Terraform stack while this
    # trap still deletes the namespace leaves a cluster that is billing and has
    # no events, no pods and no claim left to examine -- which is the whole of
    # what a post-mortem wants.
    if [ "$keep" = "1" ]; then
        echo "gate-aws: YESNO_AWS_KEEP=1; leaving namespace $namespace and its objects in place"
        return
    fi

    if [ -n "$token" ]; then
        # An interrupted run leaves a Job holding a CSI-provisioned volume.
        # Deleting the namespace releases the claim, which is what deletes
        # that volume; the retained snapshot contents are cluster-scoped and
        # go separately. Neither touches the EBS snapshot, which the server
        # owns and deletes on lease release.
        curl -sS $curl_timeouts -o /dev/null --cacert "$kube_dir/ca.crt" -H "Authorization: Bearer $token" -X DELETE "$endpoint/apis/snapshot.storage.k8s.io/v1/volumesnapshotcontents?labelSelector=app.kubernetes.io%2Fmanaged-by%3Dyesno-archive" >/dev/null 2>&1 || true
        curl -sS $curl_timeouts -o /dev/null --cacert "$kube_dir/ca.crt" -H "Authorization: Bearer $token" -X DELETE "$endpoint/api/v1/namespaces/$namespace" >/dev/null 2>&1 || true
    fi
    docker run $common "$image" "$cleanup_command" || true
}
trap cleanup EXIT

docker run $common \
    --mount "type=bind,source=$source_mount,target=$source_mount" \
    --mount "type=bind,source=$staging_mount,target=$staging_mount" \
    --mount "type=bind,source=$kube_dir,target=$kube_dir,readonly" \
    "$image" \
    "$harness" --timeout "$timeout" "$scenario"
"""

OPERATOR_BODY = """
# No source mount, no staging mount, no snapshot mount, no /dev, and no
# privilege. The operator arm touches nothing on this host: its database runs
# on an EKS node, on a volume the EBS CSI driver provisions, and the only thing
# this container does is drive the Kubernetes API. A scenario that quietly
# needed a mount fails here rather than borrowing one.

# The harness's own `diagnose()` already dumps the objects and both logs
# through kubectl when a verb fails, so this trap does not repeat it. What it
# does is the one thing the harness deliberately does not: remove the arm's
# namespace so its claims -- which the controller does **not** owner-reference,
# because a database claim must survive its YesnoCluster -- release the EBS
# volumes the CSI driver made, before `terraform destroy` takes the cluster
# that would have deleted them.
#
# Waiting matters as much as deleting. Namespace deletion returns once the
# claims are gone; the volumes go a moment later, and destroying the cluster in
# between orphans them. `scripts/gate-aws-destroy.sh --orphans` is the backstop
# and this is the thing that should make it unnecessary.
#
# `kubectl` runs *in the container*. There is none on this host -- the
# deferred arms reach the same API server with `curl` for exactly that reason --
# and the image the harness runs from carries one, so the delete goes the same
# way the scenario did.
kube_mount="--mount type=bind,source=$kube_dir,target=$kube_dir,readonly"

cleanup() {
    if [ "$keep" = "1" ]; then
        echo "gate-aws: YESNO_AWS_KEEP=1; leaving namespace $namespace, its database and its volumes in place"
        return
    fi
    docker run $common $kube_mount "$image" \\
        kubectl --kubeconfig "$operator_kubeconfig" delete namespace "$namespace" \\
        --ignore-not-found --wait=true --timeout=600s || true
    # The namespace being gone is not the volumes being gone. Deleting it
    # removes the claims; the CSI driver then deletes each volume, and a
    # `terraform destroy` that takes the cluster away in between orphans them --
    # billing, and fatal to the *next* run, whose precondition counts by a tag
    # that is a constant.
    remaining=1
    attempt=0
    while [ "$attempt" -lt 60 ]; do
        remaining="$(aws ec2 describe-volumes --region "$region" \\
            --filters "Name=tag:kubernetes.io/created-for/pvc/namespace,Values=$namespace" \\
            --query 'length(Volumes)' --output text 2>/dev/null || echo unknown)"
        if [ "$remaining" = "0" ]; then
            break
        fi
        attempt=$((attempt + 1))
        sleep 5
    done
    if [ "$remaining" != "0" ]; then
        echo "gate-aws: $remaining CSI volume(s) still present for namespace $namespace" >&2
        echo "gate-aws: clear them with scripts/gate-aws-destroy.sh --orphans" >&2
    fi
}
trap cleanup EXIT

docker run $common $kube_mount \\
    "$image" \\
    "$harness" --timeout "$timeout" "$scenario"
"""


def summary_of(result):
    """The runner's own tally line, or "" if it never printed one.

    A zero exit with nothing executed is the shape a status check alone cannot
    tell from a pass, and it is exactly what a stale image or a scenario file
    missing from it would produce.
    """
    found = ""
    for line in result["stdout"].split("\n"):
        if " scenario(s): " in line:
            found = line.strip()
    return found


def prologue(names):
    """The `name=value` lines a runner script opens with.

    Keeping every varying value here is what lets each body below read as the
    POSIX shell it is, with no interpolation anywhere near an awk brace or a
    JSON document.
    """
    lines = "set -eu\n"
    for name in sorted(names):
        value = names[name]
        assert value != "" and "\n" not in value, (name, value)
        lines = lines + name + "=" + value + "\n"
    return lines


def env_flags(pairs):
    """`docker run` flags built by iterating what the host says is required.

    Never by naming the variables here: the two halves of this gate run on
    different machines and a variable added on the far side must not be able to
    go missing on this one.
    """
    flags = []
    for name in sorted(pairs):
        value = pairs[name]
        # `docker run $common` below is deliberately unquoted, which is only
        # safe while every value is a single word. All of them are AWS
        # identifiers, ARNs or absolute paths; this is the check, not the
        # assumption.
        assert value != "" and " " not in value, (name, value)
        flags.append("-e " + name + "=" + value)
    return flags


config = cloud_config()
assert config["run_id"], config
assert config["architecture"] in ("x86_64", "arm64"), config
# Which arms this run executes. The host validated it; this restates the
# set so a value that reached here unrecognised stops the run rather than
# matching no branch and reporting a pass over nothing.
only = config["only"]
assert only in ("", "local", "ecs", "eks", "operator"), only
# Whether to leave everything standing for a post-mortem. Passed down into
# every runner script, because the stack is the least of it: each arm's cleanup
# trap removes the workers, the mounts and -- for EKS -- the whole namespace,
# which is where the events and the failed pod live.
keep = "1" if config["keep"] else "0"

assert cloud_terraform("init") is True
assert cloud_terraform("validate") is True
assert cloud_terraform("apply") is True

instance = cloud_output("runner_instance_id")
volume = cloud_output("source_volume_id")
zone = cloud_output("availability_zone")
image = cloud_output("driver_image")
platform = cloud_output("driver_platform")
efs_id = cloud_output("efs_file_system_id")
staging_mount = cloud_output("staging_mount")
cluster = cloud_output("ecs_cluster")

assert instance.startswith("i-"), instance
assert volume.startswith("vol-"), volume
# A volume and an instance in different zones cannot be attached to each other
# at all, so the zone has to be inside the region the image is pushed to.
assert zone.startswith(config["region"]), zone
assert platform in ("linux/amd64", "linux/arm64"), platform
assert efs_id.startswith("fs-"), efs_id
assert staging_mount.startswith("/"), staging_mount
# Every resource a run creates is named or tagged with its run id, which is
# what makes both the failure-path cleanup and the IAM policy exact. An ECR tag
# that did not carry it would let one run pull another run's image.
assert config["run_id"] in image, image
assert config["run_id"] in cluster, cluster

assert cloud_push_image(RUNNER_DOCKERFILE, image, platform, RUNNER_TARGET) is True

# Whether this run drives the operator. Both conditions: it manages a
# YesnoCluster inside the EKS cluster, so YESNO_AWS_EKS=0 leaves it nothing to
# run against, and the host refuses that combination before anything is applied.
run_operator = config["eks"] and only in ("", "operator")
if run_operator:
    yesnod_image = cloud_output("yesnod_image")
    operator_image = cloud_output("operator_image")
    # One repository, three tags, and every layer but the last already pushed
    # above -- so these two cost a manifest each.
    assert config["run_id"] in yesnod_image, yesnod_image
    assert config["run_id"] in operator_image, operator_image
    assert yesnod_image != image and operator_image != image, yesnod_image
    assert cloud_push_image(RUNNER_DOCKERFILE, yesnod_image, platform, YESNOD_TARGET) is True
    assert cloud_push_image(RUNNER_DOCKERFILE, operator_image, platform, OPERATOR_TARGET) is True

# The gate opens no ingress port at all; Systems Manager reaches the runner
# over its own outbound channel. Nothing can be asked of the instance until it
# has registered, so this is the boundary rather than a courtesy sleep.
assert cloud_wait_managed(instance, 600) is True

provision = prologue({
    "efs_id": efs_id,
    "region": config["region"],
    "snapshot_mount": SNAPSHOT_MOUNT,
    "source_mount": SOURCE_MOUNT,
    "staging_mount": staging_mount,
    "token": volume.replace("-", ""),
    "volume_id": volume,
}) + PROVISION_BODY
provisioned = cloud_run(instance, provision, 900)
assert provisioned["status"] == "Success", provisioned
assert provisioned["exit_code"] == 0, provisioned

# Resolving the device by the volume's own id is the step most likely to break
# on a new instance family, so the runner reports what it resolved and this
# asserts it. Otherwise that failure arrives later, as an unmountable
# filesystem, with nothing pointing at the cause.
device = ""
for line in provisioned["stdout"].split("\n"):
    if line.startswith("yesno-device="):
        device = line[len("yesno-device="):].strip()
assert device.startswith("/dev/"), provisioned

runner_env = cloud_runner_env(instance, volume, zone, SOURCE_MOUNT + ":" + SNAPSHOT_MOUNT)
assert len(runner_env) > 0, runner_env
runner_flags = env_flags(runner_env)


def deferred_arm(materializer, scenario, seconds, body, extra):
    """Ship one deferred arm to the runner and require its scenario to pass.

    The two arms differ in the variables `yesno-archive` needs and in what
    their container has to see, and in nothing else. Composing both here is
    what stops a change to how an arm is delivered from reaching only one of
    them.
    """
    env = cloud_deferred_env(materializer)
    assert len(env) > 0, materializer
    for name in runner_env:
        # The two lists describe different machines' contracts and must not
        # overlap: a name in both would have two sources and no owner.
        assert name not in env, name
    flags = runner_flags + env_flags(env)
    names = {
        "cleanup_command": CLEANUP_COMMAND,
        "harness": HARNESS,
        "image": image,
        "keep": keep,
        "scenario": scenario,
        "source_mount": SOURCE_MOUNT,
        "staging_mount": staging_mount,
        "timeout": str(seconds),
    }
    for name in extra:
        names[name] = extra[name]
    common = "common='--rm --network host " + " ".join(flags) + "'\n"
    script = prologue(names) + common + body
    # Systems Manager must outlive the harness it is running, or it would
    # cancel a scenario that is still going and report a timeout instead of
    # the failure.
    result = cloud_run(instance, script, seconds + 300)
    print(result["stdout"])
    print(result["stderr"])
    assert result["status"] == "Success", result
    assert result["exit_code"] == 0, result
    assert summary_of(result) == "1 scenario(s): 1 passed, 0 failed", result


if only in ("", "local"):
    ebs_names = {
        "cleanup_command": CLEANUP_COMMAND,
        "harness": HARNESS,
        "image": image,
        "keep": keep,
        "scenario": EBS_SCENARIO,
        "snapshot_mount": SNAPSHOT_MOUNT,
        "source_mount": SOURCE_MOUNT,
        "timeout": str(EBS_SECONDS),
    }
    ebs_common = "common='--rm --network host " + " ".join(runner_flags) + "'\n"
    ebs_run = prologue(ebs_names) + ebs_common + EBS_BODY
    result = cloud_run(instance, ebs_run, EBS_SECONDS + 300)
    print(result["stdout"])
    print(result["stderr"])
    assert result["status"] == "Success", result
    assert result["exit_code"] == 0, result
    assert summary_of(result) == "1 scenario(s): 1 passed, 0 failed", result
else:
    print("gate-aws: YESNO_AWS_ONLY=" + only + "; the local EBS arm was not run")

# The first deferred arm. Same instance, same volume, same image -- what
# differs is that yesnod hands out a provisional snapshot instead of mounting
# one, and an ECS task materializes it.
if only in ("", "ecs"):
    deferred_arm("ecs", ECS_SCENARIO, ECS_SECONDS, ECS_BODY, {"cluster": cluster})
else:
    print("gate-aws: YESNO_AWS_ONLY=" + only + "; the deferred ECS arm was not run")

# The second, if this run provisioned it. YESNO_AWS_EKS=0 buys back the
# fifteen minutes a cluster takes to create and the ten it takes to destroy,
# for a developer iterating on something else.
#
# Every EKS output is read *inside* this branch. Three of them come from a
# resource that may not exist, and `cloud_output()` refuses an empty value --
# reading them unconditionally would fail a run that deliberately skipped the
# arm. The flag comes from cloud_config() rather than from this file, because
# the stack and the wrapper's destroy have to agree with whatever it says.
#
# The bootstrap is shared with the operator arm and runs when *either* is
# selected, because both need a credential it mints. It is idempotent -- every
# POST tolerates the 409 a retried run produces -- but it is not run twice: one
# branch covers both arms and each is then guarded on its own.
if config["eks"] and only in ("", "eks", "operator"):
    eks_cluster = cloud_output("eks_cluster_name")
    eks_endpoint = cloud_output("eks_endpoint")
    eks_ca = cloud_output("eks_certificate_authority")
    eks_namespace = cloud_output("eks_namespace")
    eks_account = cloud_output("eks_archive_account")
    kubeconfig = cloud_output("kubeconfig_path")
    operator_kubeconfig = cloud_output("operator_kubeconfig_path")

    assert config["run_id"] in eks_cluster, eks_cluster
    assert eks_endpoint.startswith("https://"), eks_endpoint
    assert eks_ca != "", eks_ca
    assert kubeconfig.startswith("/"), kubeconfig
    # Two files, never one. The archiver's token is namespace-scoped by
    # design and could not install a CRD; the operator arm's is administrative
    # and must never become what the archiver runs under, which is the whole
    # statement its scoped RBAC makes.
    assert operator_kubeconfig.startswith("/"), operator_kubeconfig
    assert operator_kubeconfig != kubeconfig, operator_kubeconfig

    # The directory the bootstrap writes into and the container reads. Derived
    # rather than another output: the kubeconfigs and the CA have to be one bind
    # mount, so the path is the kubeconfig's own parent by construction.
    kube_dir = kubeconfig[:kubeconfig.rfind("/")]
    assert kube_dir.startswith("/") and kube_dir != "/", kubeconfig
    # Both kubeconfigs have to land inside the one mount, or the container gets
    # a path that exists on the host and not in it.
    assert operator_kubeconfig.startswith(kube_dir + "/"), operator_kubeconfig

    # Everything in the cluster that the archiver needs is created first, by
    # the runner, against the Kubernetes API -- there is no kubectl on the
    # instance and no Kubernetes provider in the stack.
    bootstrap_script = prologue({
        "account": eks_account,
        "ca_data": eks_ca,
        "cluster": eks_cluster,
        "efs_id": efs_id,
        "endpoint": eks_endpoint,
        "installer": OPERATOR_INSTALLER,
        "kube_dir": kube_dir,
        "kubeconfig": kubeconfig,
        "namespace": eks_namespace,
        "operator_kubeconfig": operator_kubeconfig,
        "region": config["region"],
        "snapshot_class": cloud_output("eks_snapshot_class"),
        "staging_claim": cloud_output("eks_staging_claim"),
        "storage_class": cloud_output("eks_storage_class"),
    }) + EKS_BOOTSTRAP_BODY
    bootstrap = cloud_run(instance, bootstrap_script, 1800)
    assert bootstrap["status"] == "Success", bootstrap
    assert bootstrap["exit_code"] == 0, bootstrap
    # The kubeconfigs are the artefacts of that step, and both fail obscurely
    # when missing -- the archiver's `Client::try_default()` would fall back to
    # localhost:8080 several minutes into the run, and kubectl would report a
    # connection refused against a cluster nobody named.
    written = {}
    for line in bootstrap["stdout"].split("\n"):
        for prefix in ("yesno-kubeconfig=", "yesno-operator-kubeconfig="):
            if line.startswith(prefix):
                written[prefix] = line[len(prefix):].strip()
    assert written.get("yesno-kubeconfig=") == kubeconfig, bootstrap
    assert written.get("yesno-operator-kubeconfig=") == operator_kubeconfig, bootstrap

    if only in ("", "eks"):
        deferred_arm(
            "eks",
            EKS_SCENARIO,
            EKS_SECONDS,
            EKS_BODY,
            {
                "cluster": eks_cluster,
                "endpoint": eks_endpoint,
                "kube_dir": kube_dir,
                "namespace": eks_namespace,
                "region": config["region"],
            },
        )
    else:
        print("gate-aws: YESNO_AWS_ONLY=" + only + "; the deferred EKS arm was not run")
elif config["eks"]:
    print("gate-aws: YESNO_AWS_ONLY=" + only + "; the deferred EKS arm was not run")
else:
    print("gate-aws: YESNO_AWS_EKS=0; the deferred EKS arm was not provisioned")

# ---------------------------------------------------------------------------
# The operator arm. Same cluster, same storage class, and a database this file
# never configures: the operator writes its configuration, and the volume id in
# it comes from the PersistentVolume the claim bound to.
#
# This is the only arm where an EBS snapshot backend is *generated* rather
# than written by hand, and the only place the operator's discovery meets a
# real `vol-`. The kind gate exercises the same code path up to the answer and
# can go no further, because kind provisions local-path volumes.
# ---------------------------------------------------------------------------
if run_operator:
    operator_env = cloud_operator_env()
    assert len(operator_env) > 0, operator_env
    for name in runner_env:
        # Different machines' contracts, exactly as the deferred arms' are: a
        # name in both would have two sources and no owner.
        assert name not in operator_env, name
    # The images the cluster pulls have to be the ones this run pushed, and the
    # entrypoint-bearing tags rather than the harness's own.
    assert operator_env["YESNO_OPERATOR_SERVER_IMAGE"] == yesnod_image, operator_env
    assert operator_env["YESNO_OPERATOR_IMAGE"] == operator_image, operator_env
    assert operator_env["YESNO_OPERATOR_KUBECONFIG"] == operator_kubeconfig, operator_env
    operator_namespace = operator_env["YESNO_OPERATOR_NAMESPACE"]
    assert operator_namespace != eks_namespace, operator_namespace

    operator_flags = env_flags(operator_env)
    operator_names = {
        "harness": HARNESS,
        "image": image,
        "keep": keep,
        "kube_dir": kube_dir,
        "namespace": operator_namespace,
        "operator_kubeconfig": operator_kubeconfig,
        "region": config["region"],
        "scenario": OPERATOR_SCENARIO,
        "timeout": str(OPERATOR_SECONDS),
    }
    operator_common = "common='--rm --network host " + " ".join(operator_flags) + "'\n"
    operator_script = prologue(operator_names) + operator_common + OPERATOR_BODY
    result = cloud_run(instance, operator_script, OPERATOR_SECONDS + 300)
    print(result["stdout"])
    print(result["stderr"])
    assert result["status"] == "Success", result
    assert result["exit_code"] == 0, result
    assert summary_of(result) == "1 scenario(s): 1 passed, 0 failed", result
elif config["eks"]:
    print("gate-aws: YESNO_AWS_ONLY=" + only + "; the operator arm was not run")
else:
    print("gate-aws: YESNO_AWS_EKS=0; the operator arm was not provisioned")

# Said out loud at the end, where it is read, because a passing gate that ran
# one arm must not look like a passing gate. TODO.md's live-gate item is
# advanced by neither of these.
if only != "":
    print("gate-aws: PARTIAL RUN -- YESNO_AWS_ONLY=" + only + " ran that arm and no other")
if not config["eks"]:
    print("gate-aws: PARTIAL RUN -- YESNO_AWS_EKS=0 left the EKS arm unprovisioned")

if keep == "1":
    # Said in full, because a retained run bills until someone acts and every
    # path needed to reach it is per-run. The wrapper's trap and the harness's
    # own destructor stand down for the same flag, so nothing else will print it.
    print("gate-aws: YESNO_AWS_KEEP=1; run " + config["run_id"] + " is RETAINED and billing.")
    print("gate-aws:   reach the runner with: aws ssm start-session --target " + instance)
    if config["eks"] and only in ("", "eks", "operator"):
        print("gate-aws:   cluster " + eks_cluster + ", kubeconfig on the runner at " + kubeconfig)
        print("gate-aws:   operator kubeconfig at " + operator_kubeconfig)
    print("gate-aws:   tear it down with: scripts/gate-aws-destroy.sh " + config["run_id"])
else:
    assert cloud_terraform("destroy") is True
