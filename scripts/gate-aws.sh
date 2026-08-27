#!/usr/bin/env bash
#
# Opt-in real-AWS EBS gate: local materialization, both deferred arms, and the
# operator's EBS snapshot backend against real Amazon EBS volumes. The
# orchestration lives in e2e/aws/gate.py and the cloud_* verbs behind it —
# Terraform apply, the ECR push, the Systems Manager round trips, and destroy.
# This wrapper does the two things a scenario cannot.
#
# One: it opts in. Every cloud_* verb refuses without YESNO_AWS_GATE=1, so that
# `cargo test -p yesno-e2e` — which calls every advertised verb with no
# arguments — cannot provision billable infrastructure.
#
# Two: it guarantees destroy across a signal. The harness destroys the stack in
# cloud_cleanup(), and again from a destructor if the scenario failed before
# reaching it, but a destructor does not run on SIGINT or SIGTERM. The trap
# below is the backstop for exactly that case; on an already-destroyed run it
# is a fast no-op against an empty state file.
#
# YESNO_AWS_EKS=0 skips both Kubernetes arms — deferred materialization and
# the operator — buying back the fifteen minutes a cluster takes to create and
# the ten it takes to destroy. It is read by the harness, not here, and recorded
# in the run's gate.tfvars — so the trap below destroys exactly what the apply
# built. A run with it set is not a run of this gate: it advances neither of
# TODO.md's live Kubernetes items, and it must not become a CI default.
#
# YESNO_AWS_ONLY=local|ecs|eks|operator runs that one arm and skips the
# others. The stack, the image and the runner are still built in full -- what it
# buys back is the roughly twelve minutes the passing arms take to re-prove
# themselves while another is being debugged. `YESNO_AWS_ONLY=eks` is the fast
# loop on the Kubernetes bootstrap and `YESNO_AWS_ONLY=operator` the fast loop
# on the operator arm; both still pay for the cluster.
#
# `eks` and `operator` each need that cluster, so either one together with
# YESNO_AWS_EKS=0 is refused rather than quietly skipped.
#
# Unlike YESNO_AWS_EKS, an unrecognised value is an error rather than a
# no-op: a switch that skips work must not be able to skip all of it on a typo
# and report a pass. A run with it set is likewise not a run of this gate, and
# the scenario says so on its last line.
#
# YESNO_AWS_KEEP=1 leaves everything standing for a post-mortem: the stack,
# the runner, the Fargate task, and -- for EKS -- the namespace with its events
# and its failed pod. It is not enough to keep only the Terraform stack: each
# arm's cleanup trap deletes its own workers as it exits, so the flag is passed
# down into every runner script too. A retained run bills until removed, and the
# scenario prints how to reach it and how to tear it down.
#
# If teardown ever falls short — both destroy paths run with the same
# credentials at the end of the same run, so one expired session takes out both
# — `scripts/gate-aws-destroy.sh` is the escape hatch. With no arguments it
# destroys every run whose state still holds resources.

set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.." || exit 1
. "$PWD/scripts/scratch.sh"

region="${AWS_REGION:-${AWS_DEFAULT_REGION:-}}"
if [[ -z "$region" ]]; then
    echo "gate-aws: set AWS_REGION or AWS_DEFAULT_REGION" >&2
    exit 2
fi

run_id="${YESNO_AWS_RUN_ID:-yn-$(date -u +%Y%m%d%H%M%S)-$$}"
state_dir="$YESNO_SCRATCH_DIR/aws-$run_id"
mkdir -p "$state_dir"

export YESNO_AWS_GATE=1
export YESNO_AWS_RUN_ID="$run_id"
export YESNO_AWS_STATE_DIR="$state_dir"
export AWS_REGION="$region"

destroy() {
    if [[ "${YESNO_AWS_KEEP:-0}" == "1" ]]; then
        echo "gate-aws: YESNO_AWS_KEEP=1; retaining run $run_id" >&2
        return
    fi
    # Delegated so the destroy has exactly one definition, and so a failure
    # is visible. This used to restate the command and silence it with
    # `>/dev/null 2>&1 || true`, which is how a teardown defeated by an expired
    # SSO session on 2026-09-02 reported nothing at all while 65 resources
    # including an EKS cluster stayed up for over an hour.
    #
    # The script is a no-op on a run the harness already destroyed, which is the
    # common case: this trap fires on every exit, successful or not.
    if ! scripts/gate-aws-destroy.sh "$run_id"; then
        echo "gate-aws: run $run_id is STILL STANDING and bills until removed." >&2
        echo "gate-aws: restore credentials, then run:" >&2
        echo "gate-aws:   scripts/gate-aws-destroy.sh $run_id" >&2
    fi
}
# Before anything is applied. The deferred scenarios open by requiring that
# no materializer volume exists, and the EKS filter keys on the namespace, which
# is a constant -- so one volume orphaned by an abandoned run fails the *next*
# run's precondition. That costs a full stack: 26 billable minutes to provision
# a cluster and reach a check that fails in thirteen seconds. Twice, on
# 2026-09-04.
#
# Advisory, not authoritative: a describe that errors reports nothing and the
# run proceeds. Blocking every run because one API call hiccuped would be the
# worse failure.
if ! scripts/gate-aws-destroy.sh --orphans --dry-run; then
    echo "gate-aws: leftover materializer resources will fail this run's" >&2
    echo "gate-aws: precondition before it asserts anything. Clear them with:" >&2
    echo "gate-aws:   scripts/gate-aws-destroy.sh --orphans" >&2
    exit 2
fi

trap destroy EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# The whole sequence — apply, push, provision, three scenario runs, destroy —
# happens inside one scenario, so the limit has to cover all of it rather than
# one RPC. An EKS cluster and node group are roughly fifteen minutes to
# create and ten to destroy on their own, before any of the three runner
# scenarios starts; the default is sized for that whole span with slack, not
# for the sum of every per-step budget. A run that exceeds it is still
# destroyed, by the trap above.
cargo run --locked -p yesno-e2e --bin yesno-e2e -- \
    --show-output \
    --timeout "${YESNO_AWS_TIMEOUT:-12000}" \
    e2e/aws/gate.py
