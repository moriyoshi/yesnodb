# The workspace scratch directory, defined once for every shell entry point.
#
# Sourced, never executed. It sets `YESNO_SCRATCH_DIR` only if the caller has
# not, so an override survives into everything the script runs -- including the
# `yesno-e2e` harness, which reads the same variable and falls back to the same
# default.
#
# Do not re-derive this path anywhere else. It was written out longhand in
# seven scripts and three Rust files, which is seven-plus-three places to keep
# in step with `.bazelrc`, with CLAUDE.md's rule about where temporary files go,
# and with each other.
#
# `.bazelrc` still spells it out, and has to: Bazel's rc files take a literal
# `--symlink_prefix` and expand no variables. It is the one copy that cannot
# read this.
: "${YESNO_SCRATCH_DIR:=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.agents-workspace/tmp}"
export YESNO_SCRATCH_DIR
