#!/usr/bin/env bash
# The escape hatch for a run whose teardown fell short.
#
# Both automatic destroy paths — the harness's `Drop` and `gate-aws.sh`'s
# EXIT trap — run in the same process tree, with the same credentials, at the
# end of the same run. A session that expires mid-flight takes out both at once,
# and what stays up is an EKS cluster billing by the hour. That happened on
# 2026-09-02: 65 resources stood for over an hour, because recovering by hand
# means reconstructing four per-run paths that no default invocation of
# Terraform would find.
#
#   scripts/gate-aws-destroy.sh              # every run that still holds resources
#   scripts/gate-aws-destroy.sh yn-... yn-.. # exactly those runs
#   scripts/gate-aws-destroy.sh --orphans    # volumes and snapshots Terraform never saw
#   scripts/gate-aws-destroy.sh --orphans --dry-run   # list them; non-zero if any
#
# Deliberately a separate entry point rather than a flag on `gate-aws.sh`:
# recovery has to work when the gate does not, and it must not be reachable by
# accident from a run that is merely slow.
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1
. "$PWD/scripts/scratch.sh"
root="$YESNO_SCRATCH_DIR"

# Does this run still own anything?
#
# The first version grepped the state file for `"resources": []`. That string
# occurs *nested inside resource attributes* -- six times in a live 65-resource
# stack on 2026-09-04 -- so this reported "no run is standing" while an EKS
# cluster billed. A substring match on a JSON document is not a query about its
# structure.
#
# It also failed the wrong way. The top-level `resources` array is the only
# thing that answers the question, so it is parsed; and when it *cannot* be
# parsed -- no python3, a truncated write, a state from a future format -- this
# answers **standing**. A needless destroy against an empty state is a fast
# no-op; a skipped destroy against a live one is a bill nobody is watching.
standing() {
    [[ -f "$1/terraform.tfstate" ]] || return 1
    python3 - "$1/terraform.tfstate" <<'PYTHON'
import json, sys
try:
    with open(sys.argv[1]) as handle:
        resources = json.load(handle).get("resources", [])
except Exception:
    sys.exit(0)          # unreadable: treat as standing
sys.exit(0 if resources else 1)
PYTHON
}

# Is a gate still using this run?
#
# This tool exists for runs whose teardown fell short, and its no-argument
# form cannot otherwise tell one of those from a run that is thirty seconds into
# `terraform apply`. Destroying a live run is the worst thing it could do, and
# on 2026-09-04 it very nearly did: a no-argument invocation found an in-flight
# run and started destroying it, stopped only by the caller having no
# credentials.
#
# Two places to look, because the harness spends most of its time in neither:
# a `terraform` holding this run's state file appears in some process's argv,
# and the harness itself carries `YESNO_AWS_STATE_DIR` in its environment even
# while it is idle waiting on Systems Manager.
#
# One `grep` per source rather than a loop over `/proc`: the loop spawns two
# processes per pid, and this runs before every destroy.
live() {
    local state_dir="$1"
    grep -qsaF -- "$state_dir" /proc/[0-9]*/cmdline && return 0
    grep -qsaF -- "YESNO_AWS_STATE_DIR=$state_dir" /proc/[0-9]*/environ && return 0
    return 1
}

destroy_one() {
    local run_id=$1
    local state_dir="$root/aws-$run_id"

    # Named explicitly or not, a live run is never destroyed. Naming one is
    # almost always a mistake, so that is an error rather than a skip.
    if live "$state_dir"; then
        echo "gate-aws-destroy: $run_id is still RUNNING; refusing to destroy it" >&2
        echo "gate-aws-destroy:   stop the gate first, then run this again" >&2
        return 1
    fi

    if [[ ! -f "$state_dir/gate.tfvars" ]]; then
        echo "gate-aws-destroy: $run_id was never applied; nothing to destroy"
        return 0
    fi
    if ! standing "$state_dir"; then
        echo "gate-aws-destroy: $run_id holds no resources"
        return 0
    fi

    echo "gate-aws-destroy: destroying $run_id"
    # The same four per-run paths the harness and the trap use. `gate.tfvars`
    # carries the region and the arm switch, so this cannot disagree with the
    # apply about what was built — which is why the file is reused rather than
    # the variables restated, and why no AWS_REGION is needed here.
    TF_DATA_DIR="$state_dir/terraform" terraform -chdir=e2e/aws destroy \
        -auto-approve -input=false \
        -state="$state_dir/terraform.tfstate" \
        -var-file="$state_dir/gate.tfvars"
}

# Resources this gate creates that Terraform never knew about.
#
# Two kinds leak past `terraform destroy`, and neither is a bug in it: the
# EBS CSI driver provisions a volume for a claim, and the yesno server creates
# snapshots and clone volumes for a lease. Terraform manages neither, so
# destroying the cluster or the instance leaves them behind, billing.
#
# Worse than the bill: the deferred scenarios refuse to start when one is
# present. Their precondition is `aws_materializer_volumes(0, 0, 0)` and the
# EKS namespace is a **constant**, not per-run -- so one volume orphaned by an
# abandoned run blocks every run after it. That happened on 2026-09-04.
#
# Only `available` volumes. An attached one belongs to a cluster that is
# still running, and this is the tool people reach for when they are already
# in trouble.
orphans() {
    local region="$1"
    local dry_run="${2:-}"
    # Both namespaces, in one filter. The deferred arm's claims are created
    # by the archiver in `yesno-e2e`; the operator arm's are created by the
    # controller in `yesno-operator-e2e`, and those are the ones that outlive a
    # `terraform destroy` most easily -- the controller deliberately does not
    # owner-reference a database claim, so nothing deletes it when the
    # YesnoCluster goes. Do not narrow this back to one namespace: the
    # constant is what makes an abandoned run block the *next* one.
    local ns_tag="Name=tag:kubernetes.io/created-for/pvc/namespace,Values=yesno-e2e,yesno-operator-e2e"
    local run_tag="Name=tag-key,Values=yesno:e2e-run"
    local available="Name=status,Values=available"
    local found=0

    # The namespace query drops the `available` filter when only listing.
    # The scenarios' precondition counts by **tag alone**, so a volume attached
    # to another cluster is fatal to a run and invisible to a pre-flight that
    # only looks at unattached ones. Deleting still requires `available`:
    # an attached volume belongs to something that is running.
    #
    # The run-tag query keeps the filter in both modes. Terraform's
    # `default_tags` put that tag on the source volume too, so listing attached
    # ones would flag every stack that is legitimately up.
    local ns_status="$available"
    [[ -n "$dry_run" ]] && ns_status=""
    for filter in "$ns_tag" "$run_tag"; do
        local status="$available"
        [[ "$filter" == "$ns_tag" ]] && status="$ns_status"
        for id in $(aws ec2 describe-volumes --region "$region" \
            --filters "$filter" ${status:+"$status"} \
            --query 'Volumes[].VolumeId' --output text 2>/dev/null); do
            if [[ -n "$dry_run" ]]; then
                echo "gate-aws-destroy: orphaned volume $id"
            else
                echo "gate-aws-destroy: deleting orphaned volume $id"
                aws ec2 delete-volume --region "$region" --volume-id "$id" || return 1
            fi
            found=$((found + 1))
        done
    done

    for id in $(aws ec2 describe-snapshots --region "$region" --owner-ids self \
        --filters "$run_tag" --query 'Snapshots[].SnapshotId' --output text 2>/dev/null); do
        if [[ -n "$dry_run" ]]; then
            echo "gate-aws-destroy: orphaned snapshot $id"
        else
            echo "gate-aws-destroy: deleting orphaned snapshot $id"
            aws ec2 delete-snapshot --region "$region" --snapshot-id "$id" || return 1
        fi
        found=$((found + 1))
    done

    if [[ -n "$dry_run" ]]; then
        # Non-zero when something is present, so a caller can gate on it.
        # This is what `gate-aws.sh` checks before it applies anything.
        [[ "$found" -eq 0 ]] && return 0
        echo "gate-aws-destroy: $found orphaned resource(s) present" >&2
        return 1
    fi
    echo "gate-aws-destroy: $found orphaned resource(s) removed"
}

if [[ "${1:-}" == "--orphans" ]]; then
    shift
    dry_run=""
    if [[ "${1:-}" == "--dry-run" ]]; then
        dry_run=1
        shift
    fi
    # The region is not a parameter of this script anywhere else -- Terraform
    # reads it from each run's gate.tfvars -- so it is taken from the most
    # recent one, and only then from the environment.
    region="${AWS_REGION:-${AWS_DEFAULT_REGION:-}}"
    if [[ -z "$region" ]]; then
        newest=$(ls -t "$root"/aws-yn-*/gate.tfvars 2>/dev/null | head -1)
        [[ -n "$newest" ]] && region=$(sed -n 's/^region *= *"\(.*\)"/\1/p' "$newest")
    fi
    if [[ -z "$region" ]]; then
        echo "gate-aws-destroy: no region; set AWS_REGION" >&2
        exit 2
    fi
    orphans "$region" "$dry_run"
    exit $?
fi

targets=()
if [[ $# -gt 0 ]]; then
    targets=("$@")
else
    running=0
    for dir in "$root"/aws-yn-*/; do
        [[ -d "$dir" ]] || continue
        if standing "${dir%/}"; then
            name=$(basename "${dir%/}")
            # Skipped, not failed: a run in flight is standing for a very
            # good reason, and the scan is what a person runs to tidy up.
            if live "${dir%/}"; then
                echo "gate-aws-destroy: ${name#aws-} is still running; leaving it alone"
                running=$((running + 1))
                continue
            fi
            targets+=("${name#aws-}")
        fi
    done
    if [[ ${#targets[@]} -eq 0 ]]; then
        # Not "no run is standing" when one was skipped for being live: that
        # is the difference between "nothing to do" and "something is here and
        # I deliberately left it", and only the second warrants coming back.
        if [[ "$running" -gt 0 ]]; then
            echo "gate-aws-destroy: nothing to destroy; $running run(s) still in flight"
        else
            echo "gate-aws-destroy: no run is standing"
        fi
        exit 0
    fi
    echo "gate-aws-destroy: ${#targets[@]} run(s) still standing: ${targets[*]}"
fi

failed=()
for run_id in "${targets[@]}"; do
    destroy_one "$run_id" || failed+=("$run_id")
done

if [[ ${#failed[@]} -gt 0 ]]; then
    # Never silently. The whole reason this script exists is that a teardown
    # failed quietly once.
    echo "gate-aws-destroy: STILL STANDING: ${failed[*]}" >&2
    exit 1
fi
