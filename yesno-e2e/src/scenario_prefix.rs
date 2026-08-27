// Shared by `world.rs` and `fixture_world.rs` through `include!`, because the
// two are roots of **different crates**: `lib.rs` declares `pub mod world;`
// while `fixture_lib.rs` declares `pub mod world { include!("fixture_world.rs"); }`.
//
// `fixture_lib.rs` is a **Bazel-only** target. Cargo does not build it, so
// `cargo clippy --workspace --all-targets` cannot see it — a first attempt had
// `fixture_world.rs` call `crate::world::scenario_prefix`, which passed the whole
// cargo gate and failed the Bazel build with `cannot find function`. That is the
// concrete case AGENTS.md means by "neither gate subsumes the other".
//
// Do not turn this into a `mod` declaration: it would have to be added to both
// crate roots, and `fixture_lib.rs` deliberately carries a minimal tree.

/// A findable, sweepable name for a scenario's temporary root.
///
/// `tempfile::tempdir()` produces `/tmp/.tmpXXXXXX`, which is both hidden and
/// anonymous. A scenario that fails mid-way skips its teardown, so that
/// directory survives — measured at 49 directories totalling 4.2 GB accumulated
/// over five weeks. Retention is *wanted*: investigating an intermittent PITR
/// failure, the restored directory surviving was the most useful thing there
/// was. The gap was that nothing tied a directory to the run that produced it,
/// so it could neither be found deliberately nor reclaimed.
///
/// The label is `yesno-e2e-<scenario>-<epoch seconds>-`, which sorts by age and
/// greps by scenario. Epoch seconds rather than a formatted date on purpose:
/// this crate has no date formatter and adding one for a directory name would be
/// a dependency for decoration.
pub(crate) fn scenario_prefix(label: &str) -> String {
    let stem = label.rsplit('/').next().unwrap_or(label);
    let stem = stem.strip_suffix(".py").unwrap_or(stem);
    let safe: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("yesno-e2e-{safe}-{secs}-")
}
