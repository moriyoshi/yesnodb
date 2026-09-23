//! Capture point for the hotspot observer, published as `tracing` events.
//!
//! Research capture, and semver-exempt for that reason.
//!
//! The module is unconditional on purpose: gating it on `tracing` broke the
//! Bazel build, which compiles this crate without the feature while
//! `yesno-flight` calls into it. Only the emission inside is gated. This
//! rationale belongs here rather than on the `pub mod` declaration -- an
//! outer doc comment there merges with this block and makes rustdoc resolve
//! every link below in the crate root's scope, where none of these items are
//! named.
//!
//! # Why this exists
//!
//! Two open questions -- GPU residency in
//! `LTM/gpu-offload-on-unified-memory.md`, and
//! `jit-auto-min-chunks-rests-on-a-withdrawn-number` in `TODO.md` -- are
//! blocked on one unmeasured distribution: how often a key recurs on one
//! worker before that worker's cache saturates. A simulator answered the
//! *policy* half; only a real query stream can answer the workload half, and
//! capturing one means being on the path a real query takes.
//!
//! # Stability, and the rule this sits against
//!
//! **Semver-exempt**, on the [`crate::unstable_arrow`] precedent: this is a
//! research capture, its shape will change as the questions above are
//! answered, and it should be deleted once they are. Do not build on it.
//!
//! CLAUDE.md says research does not ship, and names `stats.rs` -- a 1 600-line
//! instrument in this crate with no callers -- as the precedent for why. This
//! module is a deliberate exception, placed here by an explicit decision
//! rather than by drift, and it differs from that precedent in the way that
//! matters: **it has callers.** `yesno-flight` invokes it on the live query
//! path, and the whole point of it is to run in a deployed server.
//!
//! Two things keep the exception honest. It is inert unless a subscriber asks
//! for target [`TARGET`], which costs a level check against a global atomic;
//! and a capture is bounded, so even switched on it stops by itself. What it
//! must not become is permanent: when the recurrence distribution is known,
//! delete the module, the `hotspot` line in ARCHITECTURE's diagram, and the
//! two call sites in `yesno-flight`.
//!
//! # Why it lives here and not in `yesno-flight`
//!
//! It started in `yesno-flight`, next to the query path it hooks. It belongs
//! in the core because *what it measures* is a core concern -- posting-list
//! and container recurrence, and the planned-shape recurrence that decides
//! `jit.rs`'s own admission threshold -- and none of that is about Arrow
//! Flight. The one genuinely wire-shaped part, turning a `SetExpr` into the
//! posting-list keys it names, stays at the call site: [`record_containers`]
//! takes the keys already extracted, which is why this crate needs no
//! dependency on `yesno-wire` and gets none.
//!
//! # Why `tracing` and not a file of its own
//!
//! The first version wrote bespoke files behind an environment variable, with
//! its own pid-stamped naming and a per-thread file table. That was a second
//! output channel invented next to a configured one: this crate already spans
//! every request, already emits structured events, and the server already
//! wires an `EnvFilter` plus an OTLP layer. Filtering, routing and retention
//! are solved there, by an operator who knows their deployment.
//!
//! Three things fell out of the change rather than being argued for:
//!
//! * **The payload shrank by about three orders of magnitude.** Container keys
//!   are `hash( key, i )` for `i` in `0..chunk_count`, which is *entirely
//!   determined* by `( key, chunk_count )`. The old capture expanded up to
//!   4096 hashes per leaf; this emits the two integers and lets the observer
//!   expand them. That flaw had nothing to do with transport and would have
//!   survived unnoticed in a file.
//! * **The cross-process hash contract disappeared.** Container identity is
//!   now the posting-list key itself, so nothing has to agree about hashing.
//!   Only `shape_key` still hashes, and its value is produced and consumed
//!   as an opaque id.
//! * **The hand-rolled enabled-flag disappeared.** `tracing`'s macros check
//!   callsite interest *before* evaluating field expressions, so the
//!   per-key plan build below is skipped when the target is filtered out --
//!   the same optimization the previous version hand-wrote, and got wrong once
//!   ( a two-state flag that could not tell "off" from "unresolved", so every
//!   idle query paid an out-of-line call ).
//!
//! # Two hazards that are specific to this data
//!
//! **Do not read these off a sampled exporter.** The OTLP layer applies a
//! span sampler; the `fmt` layer applies an `EnvFilter`, which is
//! target-and-level based and deterministic. A reuse-distance histogram built
//! from a 10%-sampled stream is not a 10% error -- sampling removes the
//! intervening accesses that *define* stack distance, so it reports distances
//! far shorter than the truth and hit rates far higher. Capture from the
//! filtered stream, never the sampled one.
//!
//! # A capture is bounded, and that is load-bearing
//!
//! An enabled target that emits once per query for as long as it is on is an
//! open-ended commitment: unbounded log volume on a server that may serve
//! thousands of queries a second, for a measurement that stops improving long
//! before the operator remembers to turn it off. So a capture spends a
//! **budget** -- [`CAPTURE_QUERIES`] -- and disarms itself, emitting
//! `hotspot.done` on the way out. After that the path is a spent atomic
//! counter check whatever the filter says.
//!
//! **A bounded contiguous window is the right sample here, and a random one is
//! not.** Stack distance is defined by the accesses that fall *between* two
//! touches of a key, so dropping one query in ten does not add ten percent of
//! error -- it deletes the very quantity being measured and reports distances
//! far shorter than the truth. A contiguous prefix of the stream measures
//! every reuse distance below its own length exactly. That is why this
//! throttles by stopping rather than by sampling, and why the OTLP span
//! sampler must not be the thing carrying these events.
//!
//! # What it emits, on target [`TARGET`]
//!
//! * `hotspot.key` -- `key` and `chunks`, **once per posting list per
//!   capture**. Chunk count is a property of the key, not of the query, and
//!   obtaining it costs a `key_expr` plan build; emitting it per query was
//!   most of the measured cost of having this switched on.
//! * `hotspot.query` -- `keys`, comma-separated, one event per counted query.
//!   This is now a tree walk and a format, with no snapshot work at all. The
//!   observer joins it against the `hotspot.key` events and ignores any key it
//!   has no dimensions for, which is exactly the set that is not offload
//!   material.
//! * `hotspot.shape` -- `shape`, the planned-shape fingerprint, and `thread`,
//!   a dense per-thread ordinal. **Per thread because `DagJit`'s cache is
//!   thread-local**: a workload repeating one query across 64 workers
//!   amortizes nothing, and a stream that loses the thread would report the
//!   exact opposite. The TODO names this as the sharpening that makes the
//!   measurement worth taking.
//! * `hotspot.done` -- once, when a budget is spent.
//!
//! Consumed by the offline observer, which parses whatever the operator's
//! subscriber formatted. That tool is research and deliberately outside the
//! tree, under the agent scratch directory; this module is the half that has to be
//! where the queries are.
//!
//! # Fidelity, stated rather than assumed
//!
//! A leaf contributes `chunk_count` containers rather than its actual
//! prefixes, because enumerating those needs a stream walk -- I/O per query,
//! which would change the behaviour being measured. So a range-restricted
//! query looks like a whole-posting-list query. Only leaves whose chunks are
//! *all* bitmap count, via `ChunkSource::all_bitmap_chunks`; a mixed list
//! contributes nothing rather than a guess. And a leaf is capped at
//! [`MAX_CONTAINERS_PER_LEAF`]. All three err toward *overstating* reuse, so a
//! hit rate derived from a capture is an upper bound and must be quoted as one.
//!
//! A fourth, from measuring dimensions once: a posting list is recorded at the
//! size it had when first seen. A list that grows during a capture is counted
//! at its old width. Captures are bounded and short, so the drift is small --
//! but unlike the other three this one can err in *either* direction, and it
//! is the price of taking the plan build off the per-query path.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::{Expr, Snapshot};

/// Event target. Capture with a filter directive such as
/// `RUST_LOG=yesno::hotspot=trace`.
pub const TARGET: &str = "yesno::hotspot";

/// Queries one capture records before disarming itself.
///
/// An enabled target that emits for as long as it is on is an open-ended
/// commitment. This bounds it: roughly a quarter-million events, which
/// measures every reuse distance below its own length exactly, and then stops.
/// Re-arming means restarting the process -- deliberately, so that "I left it
/// on" cannot be the state of a production server.
pub const CAPTURE_QUERIES: u64 = 250_000;

/// Containers attributed to one leaf before truncating.
///
/// 4096 containers is 32 MiB of payload, past the largest budget the residency
/// projection sweeps. The cap now bounds the *observer's* expansion rather than
/// a line length, but it bounds it in the same place and for the same reason.
pub const MAX_CONTAINERS_PER_LEAF: u64 = 4096;

/// Deterministic mix for the shape fingerprint.
///
/// `DefaultHasher` is explicitly not stable across Rust releases or platforms,
/// and two captures taken from different builds should still be comparable.
/// Pinned by a test for that reason alone -- unlike the previous design, no
/// other process has to reproduce these values.
fn hash(parts: &[u64]) -> u64 {
    let mut acc = 0x9e37_79b9_7f4a_7c15u64;
    for &p in parts {
        acc = mix64(acc ^ p);
    }
    acc
}

fn mix64(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Containers this posting list would occupy, or `None` if it is not offload
/// material.
fn leaf_chunks(key: u64, snap: &Snapshot) -> Option<u64> {
    let Expr::Source(src) = snap.key_expr(key) else {
        // `key_expr` is documented to produce a source. If that ever changes,
        // record nothing rather than guess at the new shape.
        return None;
    };
    if src.all_bitmap_chunks() != Some(true) {
        return None;
    }
    match src.chunk_count().unwrap_or(0).min(MAX_CONTAINERS_PER_LEAF) {
        0 => None,
        n => Some(n),
    }
}

/// Fingerprint of the planned expression, leaves collapsed to one marker.
///
/// This reconstructs the equivalence `DagJit` keys on -- it separates the
/// instruction stream from the leaf vector -- because that key is
/// `Vec<Instruction>` and private. It measures the workload's shape
/// recurrence. It is not a test of the JIT and must not be cited as one.
fn shape_key(e: &Expr) -> u64 {
    let mut code = Vec::new();
    postfix(&e.plan(), &mut code);
    hash(&code)
}

fn postfix(e: &Expr, out: &mut Vec<u64>) {
    match e {
        Expr::Set(_) | Expr::Source(_) => out.push(0),
        Expr::Range(_, _) => out.push(1),
        Expr::Empty => out.push(2),
        Expr::And(a, b) => {
            postfix(a, out);
            postfix(b, out);
            out.push(3);
        }
        Expr::Or(a, b) => {
            postfix(a, out);
            postfix(b, out);
            out.push(4);
        }
        Expr::Xor(a, b) => {
            postfix(a, out);
            postfix(b, out);
            out.push(5);
        }
        Expr::AndNot(a, b) => {
            postfix(a, out);
            postfix(b, out);
            out.push(6);
        }
        Expr::Not(a, _, _) => {
            postfix(a, out);
            out.push(7);
        } // **Deliberately no wildcard.** `Expr` is `#[non_exhaustive]`, which
          // binds downstream crates but not this one -- so inside `yesno-core`
          // the compiler enforces the match, and adding an `Expr` variant breaks
          // the build here until someone assigns it a marker. That is strictly
          // better than the runtime fallback this needed while it lived in
          // `yesno-flight`, where a new variant would silently have joined an
          // existing shape class and merged two shapes the JIT compiles apart.
    }
}

/// Dense per-thread ordinal. `ThreadId::as_u64` is unstable, and an opaque
/// `Debug` rendering would differ between platforms; the observer only needs
/// to partition, so a counter is enough.
fn thread_ordinal() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static ORDINAL: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    ORDINAL.with(|v| *v)
}

/// Emission, which is the only part of this module that needs `tracing`.
///
/// # Why the module is unconditional and only the behaviour is gated
///
/// It was the other way round -- `#[cfg( feature = "tracing" )] pub mod
/// hotspot;` -- and that **broke the Bazel build**, which compiles
/// `yesno-core` without the feature while `yesno-flight` calls into this
/// module. Cargo hid it: `yesno-flight`'s `server` feature declares
/// `yesno-core/tracing`, so the implication holds there and nowhere else.
///
/// Requiring every build system to reproduce a dependency-feature implication
/// is the fragile arrangement. Exporting the functions unconditionally and
/// letting them do nothing is not: any caller compiles anywhere, and without
/// the feature [`enabled`] is a `const false` that the optimizer deletes the
/// rest of.
mod emit {
    #[cfg(feature = "tracing")]
    use super::TARGET;

    #[cfg(feature = "tracing")]
    #[inline]
    pub(super) fn enabled() -> bool {
        tracing::enabled!(target: TARGET, tracing::Level::TRACE)
    }

    #[cfg(not(feature = "tracing"))]
    #[inline]
    pub(super) fn enabled() -> bool {
        false
    }

    #[cfg(feature = "tracing")]
    pub(super) fn key(key: u64, chunks: u64) {
        tracing::event!(target: TARGET, tracing::Level::TRACE, key, chunks, "hotspot.key");
    }

    #[cfg(feature = "tracing")]
    pub(super) fn query(keys: &str) {
        tracing::event!(target: TARGET, tracing::Level::TRACE, keys, "hotspot.query");
    }

    #[cfg(feature = "tracing")]
    pub(super) fn shape(shape: u64, thread: u64) {
        tracing::event!(target: TARGET, tracing::Level::TRACE, shape, thread, "hotspot.shape");
    }

    #[cfg(feature = "tracing")]
    pub(super) fn done(stream: &'static str) {
        tracing::event!(
            target: TARGET,
            tracing::Level::TRACE,
            stream,
            recorded = super::CAPTURE_QUERIES,
            "hotspot.done"
        );
    }

    // Without the feature `enabled` is always false, so none of these can be
    // reached; they exist so every call site type-checks in both builds.
    #[cfg(not(feature = "tracing"))]
    pub(super) fn key(_: u64, _: u64) {}
    #[cfg(not(feature = "tracing"))]
    pub(super) fn query(_: &str) {}
    #[cfg(not(feature = "tracing"))]
    pub(super) fn shape(_: u64, _: u64) {}
    #[cfg(not(feature = "tracing"))]
    pub(super) fn done(_: &'static str) {}
}

#[inline]
fn emit_key(k: u64, chunks: u64) {
    emit::key(k, chunks);
}

#[inline]
fn emit_query(list: &str) {
    emit::query(list);
}

#[inline]
fn emit_shape(shape: u64, thread: u64) {
    emit::shape(shape, thread);
}

#[inline]
fn emit_done(stream: &'static str) {
    emit::done(stream);
}

/// Spend one unit of a budget, reporting whether it was still solvent.
///
/// Also reports the transition, so `hotspot.done` is emitted exactly once by
/// whichever thread takes the last unit.
fn spend(budget: &AtomicU64, what: &'static str) -> bool {
    let mut cur = budget.load(Ordering::Relaxed);
    loop {
        if cur == 0 {
            return false;
        }
        match budget.compare_exchange_weak(cur, cur - 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                if cur == 1 {
                    emit_done(what);
                }
                return true;
            }
            Err(actual) => cur = actual,
        }
    }
}

/// Emit `hotspot.key` for any key not yet described in this capture.
///
/// Once per posting list, not once per query: chunk count is a property of the
/// key, and obtaining it costs a `key_expr` plan build -- which was most of the
/// measured cost of having this target enabled. A list is therefore recorded
/// at the width it had when first seen.
fn describe(keys: &[u64], snap: &Snapshot) {
    static DESCRIBED: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    let described = DESCRIBED.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut seen) = described.lock() else {
        return;
    };
    for &k in keys {
        if !seen.insert(k) {
            continue;
        }
        // `None` is recorded as zero rather than skipped, so the key is not
        // re-examined on every subsequent query that names it.
        let chunks = leaf_chunks(k, snap).unwrap_or(0);
        emit_key(k, chunks);
    }
}

/// Whether a capture is running and would record.
///
/// Exported so a caller can skip building the key list at all. Without it the
/// wire layer would allocate a `Vec` per query to hand to a function that
/// immediately discards it, which is precisely the per-query cost this module
/// spent two rounds of measurement removing. See the module header for what
/// this check actually compiles to.
#[inline]
pub fn enabled() -> bool {
    emit::enabled()
}

/// Record the posting lists one query names.
///
/// Takes the keys already extracted rather than an expression to extract them
/// from. That is what lets this live in the core at all: the identity that
/// matters is the posting-list key, and the only place it survives is the
/// *wire* expression, whose type this crate cannot see and should not. The
/// caller does `SetExpr::keys` and hands over the result; everything that is
/// not about the wire format is here.
#[inline]
pub fn record_containers(keys: &[u64], snap: &Snapshot) {
    static BUDGET: AtomicU64 = AtomicU64::new(CAPTURE_QUERIES);
    record_containers_in(keys, snap, &BUDGET);
}

/// The body, with its budget supplied.
///
/// Split so tests can drive it with a counter of their own: the budget is
/// process-wide state, and a test that spent the real one would silently
/// disarm every test that ran after it.
fn record_containers_in(keys: &[u64], snap: &Snapshot, budget: &AtomicU64) {
    // Explicit, though `event!` would check the same thing: the work below is
    // shared by the dimension pass and the query event, and the point of the
    // guard is that neither runs when the target is off. See the module header
    // for why this is a level check against a global atomic and not a lookup.
    if !enabled() {
        return;
    }
    if keys.is_empty() {
        return;
    }
    if !spend(budget, "containers") {
        return;
    }
    describe(keys, snap);

    let mut list = String::with_capacity(keys.len() * 8);
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            list.push(',');
        }
        let _ = write!(list, "{k}");
    }
    emit_query(&list);
}

/// Record a planned shape, tagged with the thread that would compile it.
#[inline]
pub fn record_shape(expr: &Expr) {
    // Its own budget rather than a shared one: the two hooks sit on different
    // functions and not every query reaches both, so one counter would let the
    // busier stream starve the other.
    static BUDGET: AtomicU64 = AtomicU64::new(CAPTURE_QUERIES);
    record_shape_in(expr, &BUDGET);
}

fn record_shape_in(expr: &Expr, budget: &AtomicU64) {
    if !enabled() {
        return;
    }
    if !spend(budget, "shapes") {
        return;
    }
    emit_shape(shape_key(expr), thread_ordinal());
}

#[cfg(all(test, feature = "tracing"))]
mod tests {
    use super::*;
    use crate::{Db, DbOptions, OrdSet};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tracing::field::{Field, Visit};

    /// Minimal collecting subscriber.
    ///
    /// Hand-rolled rather than pulling in `tracing-subscriber` as a
    /// dev-dependency: `crate_universe` reads these manifests, so a new
    /// dependency costs a Bazel re-pin, and a short trait impl is the cheaper
    /// side of that trade.
    #[derive(Default)]
    struct Fields(HashMap<String, String>);

    impl Visit for Fields {
        fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
            self.0.insert(f.name().to_string(), format!("{v:?}"));
        }
        fn record_u64(&mut self, f: &Field, v: u64) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
        fn record_str(&mut self, f: &Field, v: &str) {
            self.0.insert(f.name().to_string(), v.to_string());
        }
    }

    struct Collector {
        events: Arc<Mutex<Vec<HashMap<String, String>>>>,
        on: bool,
    }

    impl tracing::Subscriber for Collector {
        // `sometimes` on purpose: the interest cache is global, so a
        // definitive answer here would be cached across tests that install
        // different collectors.
        fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }
        fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
            self.on && m.target() == TARGET
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, e: &tracing::Event<'_>) {
            let mut f = Fields::default();
            e.record(&mut f);
            self.events.lock().expect("lock").push(f.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn capture(on: bool, body: impl FnOnce()) -> Vec<HashMap<String, String>> {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sub = Collector {
            events: Arc::clone(&events),
            on,
        };
        tracing::subscriber::with_default(sub, body);
        let out = events.lock().expect("lock").clone();
        out
    }

    fn of<'a>(
        events: &'a [HashMap<String, String>],
        kind: &str,
    ) -> Vec<&'a HashMap<String, String>> {
        // Events are told apart by the field that only that kind carries.
        events.iter().filter(|e| e.contains_key(kind)).collect()
    }

    /// The house convention for a scratch database directory. `yesno-core`
    /// carries no `tempfile` dev-dependency and should not gain one: these
    /// manifests are what `crate_universe` reads, so a new dependency costs a
    /// Bazel re-pin.
    fn tmpdir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("yesno-hotspot-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    struct CleanDir(std::path::PathBuf);
    impl Drop for CleanDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open(dir: &std::path::Path) -> Db {
        Db::open_with(
            dir,
            DbOptions {
                shards: 2,
                ..Default::default()
            },
        )
        .expect("open")
    }

    /// Dense enough that every chunk of the posting list is a bitmap.
    fn dense(db: &Db, key: u64, chunks: u64) {
        let mut v = Vec::new();
        for prefix in 0..chunks {
            for i in 0..6000u64 {
                v.push((prefix << 16) | ((i * 7) % 65536));
            }
        }
        v.sort_unstable();
        v.dedup();
        db.insert_many(key, &v).expect("insert");
    }

    #[test]
    fn the_shape_fingerprint_is_stable_across_builds() {
        // Pinned so two captures taken from different builds stay comparable.
        assert_eq!(hash(&[0]), 16294208416658607535);
        assert_eq!(hash(&[1, 2]), 7255708332913644382);
        assert_ne!(hash(&[1, 2]), hash(&[2, 1]));
    }

    #[test]
    fn a_dense_posting_list_reports_its_chunk_count() {
        let dir = tmpdir("a_dense_posting_list_reports");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 101, 3);
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        assert_eq!(leaf_chunks(101, &snap), Some(3));
    }

    #[test]
    fn a_posting_list_that_is_not_all_bitmap_is_not_offload_material() {
        let dir = tmpdir("a_posting_list_that_is_not_a");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        db.insert_many(102, &[1u64, 5, 11, 70000, 140000])
            .expect("insert");
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        assert_eq!(leaf_chunks(102, &snap), None);
    }

    #[test]
    fn the_same_key_reports_the_same_containers_across_snapshots() {
        // Regression test for the bug this file's history records: an earlier
        // version keyed containers on the leaf's `Arc` address, and `lower`
        // builds a fresh `KeySource` per query, so identical queries produced
        // disjoint keys and the trace reported zero reuse however hot the
        // workload was. A flat histogram is exactly the outcome that retires
        // the question, so the failure would have looked like an answer.
        let dir = tmpdir("the_same_key_reports_the_sam");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 103, 2);
        db.checkpoint().expect("checkpoint");
        let first = db.snapshot().expect("snapshot");
        let second = db.snapshot().expect("snapshot");
        assert_eq!(leaf_chunks(103, &first), Some(2));
        assert_eq!(leaf_chunks(103, &first), leaf_chunks(103, &second));
    }

    #[test]
    fn a_posting_list_is_described_once_however_often_it_is_queried() {
        // The overcommit fix: chunk count is a property of the key, and
        // obtaining it costs a plan build. Emitting it per query was most of
        // the cost of having this target on.
        //
        // Key ids are unique per test because the described-set is
        // process-wide, which is what makes "once" observable at all.
        let dir = tmpdir("a_posting_list_is_described_");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 110, 4);
        dense(&db, 111, 2);
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        let e = [110u64, 111];
        let budget = AtomicU64::new(100);

        let events = capture(true, || {
            for _ in 0..5 {
                record_containers_in(&e, &snap, &budget);
            }
        });
        assert_eq!(of(&events, "keys").len(), 5, "one query event per query");
        let described = of(&events, "key");
        assert_eq!(described.len(), 2, "two posting lists, described once each");
        let mut dims: Vec<(&str, &str)> = described
            .iter()
            .map(|e| {
                (
                    e.get("key").expect("key").as_str(),
                    e.get("chunks").expect("chunks").as_str(),
                )
            })
            .collect();
        dims.sort_unstable();
        assert_eq!(dims, vec![("110", "4"), ("111", "2")]);
    }

    #[test]
    fn a_query_event_names_the_posting_lists() {
        let dir = tmpdir("a_query_event_names_the_post");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 120, 4);
        dense(&db, 121, 3);
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        let e = [120u64, 121];
        let budget = AtomicU64::new(10);

        let events = capture(true, || record_containers_in(&e, &snap, &budget));
        let q = of(&events, "keys");
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].get("keys").map(String::as_str), Some("120,121"));
    }

    #[test]
    fn a_capture_spends_its_budget_and_stops() {
        let dir = tmpdir("a_capture_spends_its_budget_");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 130, 1);
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        let e = [130u64];
        let budget = AtomicU64::new(3);

        let events = capture(true, || {
            for _ in 0..50 {
                record_containers_in(&e, &snap, &budget);
            }
        });
        assert_eq!(of(&events, "keys").len(), 3, "the budget, and not one more");
        assert_eq!(
            of(&events, "recorded").len(),
            1,
            "hotspot.done is emitted exactly once, by whoever took the last unit"
        );
    }

    #[test]
    fn spending_is_exact_under_concurrency() {
        // A racy budget would either over-emit ( unbounded, the thing being
        // fixed ) or emit `hotspot.done` more than once.
        let budget = Arc::new(AtomicU64::new(1000));
        let taken: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let b = Arc::clone(&budget);
                    scope.spawn(move || (0..500).filter(|_| spend(&b, "t")).count())
                })
                .collect();
            handles.into_iter().map(|h| h.join().expect("thread")).sum()
        });
        assert_eq!(taken, 1000);
        assert_eq!(budget.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_shape_event_carries_a_fingerprint_and_a_thread_ordinal() {
        let leaf = |n: u64| Expr::Set(Arc::new(OrdSet::from_iter_unsorted([n, n + 1])));
        let expr = leaf(0).and(leaf(10));
        let budget = AtomicU64::new(10);
        let events = capture(true, || record_shape_in(&expr, &budget));
        assert_eq!(events.len(), 1);
        events[0]
            .get("shape")
            .expect("shape field")
            .parse::<u64>()
            .expect("a decimal u64");
        events[0]
            .get("thread")
            .expect("thread field")
            .parse::<u64>()
            .expect("a decimal u64");
    }

    #[test]
    fn a_shape_ignores_which_sets_its_leaves_are() {
        let leaf = |n: u64| Expr::Set(Arc::new(OrdSet::from_iter_unsorted([n, n + 1])));
        let x = leaf(0).and(leaf(10));
        assert_eq!(shape_key(&x), shape_key(&leaf(20).and(leaf(30))));
        assert_ne!(shape_key(&x), shape_key(&leaf(0).or(leaf(10))));
    }

    #[test]
    fn threads_get_distinct_ordinals() {
        // Per thread because `DagJit`'s cache is thread-local. A stream that
        // lost the thread would report a workload spread across 64 workers as
        // though it amortized, which is the opposite of the truth.
        let seen: Vec<u64> = (0..4)
            .map(|_| std::thread::spawn(thread_ordinal))
            .map(|h| h.join().expect("thread"))
            .collect();
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "ordinals collided: {seen:?}");
    }

    #[test]
    fn a_filtered_out_target_emits_nothing_and_spends_nothing() {
        let dir = tmpdir("a_filtered_out_target_emits_");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        dense(&db, 140, 2);
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        let budget = AtomicU64::new(5);

        let events = capture(false, || {
            for _ in 0..20 {
                record_containers_in(&[140u64], &snap, &budget);
                record_shape_in(&Expr::Empty, &budget);
            }
        });
        assert!(events.is_empty());
        assert_eq!(
            budget.load(Ordering::Relaxed),
            5,
            "an idle server must not burn the budget it would need later"
        );
    }

    #[test]
    fn a_query_naming_no_posting_list_emits_no_query_event() {
        let dir = tmpdir("a_query_naming_no_posting_li");
        let _clean = CleanDir(dir.clone());
        let db = open(&dir);
        db.insert(150, 1).expect("insert");
        db.checkpoint().expect("checkpoint");
        let snap = db.snapshot().expect("snapshot");
        let budget = AtomicU64::new(5);

        let events = capture(true, || {
            record_containers_in(&[], &snap, &budget);
        });
        assert!(of(&events, "keys").is_empty());
    }
}
