//! Views: the layout cost asymmetry, and each specialised arm against its oracle.
//!
//! # Why this file exists
//!
//! `ARCHITECTURE.md` states a cost table for `Interleaved` against `Blocked` —
//! extraction `O( nnz )` against a range window, cardinality `O( nnz )` against
//! `len_in_range` — and until this file existed **nothing measured it**.
//! `TODO.md` names that failure mode exactly: a slow arm and a fast arm return
//! the same value, so no correctness layer can see a missing one, and the only
//! instrument that can is a benchmark that names the pair.
//!
//! # No bench-local reimplementation, and that is deliberate
//!
//! `setops.rs` records that a bench-local copy of `ops::run`'s merge measured
//! **7-21% faster than the identical source inside the crate** — a bias that
//! would land straight in any arm-against-oracle ratio. This file avoids it
//! entirely: every arm is selected by the **descriptor**, so both sides of every
//! comparison are the same shipped code.
//!
//! - `view_select`'s aligned arm fires for a `Blocked` stride that is a multiple
//!   of 65 536 and declines otherwise, so `262 144` against `262 143` is an A/B
//!   of two real paths through one function.
//! - `view_fold`'s oracle is "select each constituent and combine", which is
//!   spelled here out of the **public** `view_select` / `or` — the same calls
//!   `fold_via_select` makes internally, not a copy of them.
//! - `view_intersection_cardinalities` is compared with selecting every blocked
//!   row and calling the public count-only intersection on it. The arm and the
//!   oracle therefore share no bench-local container kernel.
//! - `view_expand`'s generic path is `ordinal_of` per constituent per ordinal,
//!   likewise public.
//!
//! What this file cannot do is compare the two *layouts* free of confounds.
//! The same logical data packed both ways produces different chunk counts and
//! different container kinds, and that difference **is** the thing being
//! measured rather than noise around it — a view's layout decides where the bits
//! land. Read those rows as end-to-end, not as a kernel ratio.

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use yesno_core::view::{Reduce, View, ViewSink};
use yesno_core::{ContainerKind, OrdSet};

/// Deterministic, and not an RNG dev-dependency — the convention `setops.rs`,
/// `bitmatrix.rs` and `bignum.rs` already use.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

const SETS: u32 = 4;
const LOGICAL: u64 = 200_000;
/// A multiple of 65 536, so `view_select`'s aligned arm fires.
const ALIGNED: u64 = 262_144;
/// One less, so it declines and the generic walk answers on the same data.
const UNALIGNED: u64 = 262_143;

/// Constituents dense enough that the packed form really contains bitmap
/// containers.
///
/// Asserted rather than assumed. `setops.rs` records that `binary_ops_dense`
/// once measured **array** containers while claiming bitmaps, and sent two
/// specialization attempts the wrong way. The analogue here is a fixture too
/// sparse to leave the array representation, which would make every row below a
/// measurement of `ops::array`.
fn constituents() -> Vec<OrdSet> {
    let mut st = 0x2545_f491_4f6c_dd1du64;
    (0..SETS)
        .map(|_| {
            let mut s = OrdSet::from_iter_unsorted(
                (0..LOGICAL).filter(|_| !lcg(&mut st).is_multiple_of(3)),
            );
            s.optimize();
            s
        })
        .collect()
}

fn pack(v: View, parts: &[OrdSet]) -> OrdSet {
    let mut sink = ViewSink::new(v);
    for (i, p) in parts.iter().enumerate() {
        sink.place(i as u32, p).expect("addressable");
    }
    sink.build()
}

/// Panics unless the packed set really holds the container kind the rows claim.
fn assert_has_bitmaps(s: &OrdSet, what: &str) {
    let n = s
        .chunks()
        .filter(|(_, c)| c.kind() == ContainerKind::Bitmap)
        .count();
    assert!(
        n > 0,
        "{what}: no bitmap containers; the fixture is too sparse"
    );
}

/// Extracting one constituent — the headline row of the cost table.
fn select(c: &mut Criterion) {
    let parts = constituents();
    let mut g = c.benchmark_group("view/select");

    for (name, v) in [
        ("interleaved", View::interleaved(SETS)),
        ("blocked_aligned", View::blocked(SETS, ALIGNED)),
        ("blocked_unaligned", View::blocked(SETS, UNALIGNED)),
    ] {
        let packed = pack(v, &parts);
        assert_has_bitmaps(&packed, name);
        g.bench_function(name, |b| {
            b.iter(|| black_box(&packed).view_select(black_box(&v), 2))
        });
    }
    g.finish();
}

/// Cardinality of one constituent. `Blocked` answers from `len_in_range` with
/// payload access at no more than two chunks; `Interleaved` has to walk.
fn cardinality(c: &mut Criterion) {
    let parts = constituents();
    let mut g = c.benchmark_group("view/cardinality");

    for (name, v) in [
        ("interleaved", View::interleaved(SETS)),
        ("blocked", View::blocked(SETS, ALIGNED)),
    ] {
        let packed = pack(v, &parts);
        g.bench_function(name, |b| {
            b.iter(|| black_box(&packed).view_cardinality(black_box(&v), 2))
        });
    }

    // Membership is the one question both layouts answer equally cheaply, and
    // this row is here to show that rather than to be assumed.
    for (name, v) in [
        ("contains/interleaved", View::interleaved(SETS)),
        ("contains/blocked", View::blocked(SETS, ALIGNED)),
    ] {
        let packed = pack(v, &parts);
        g.bench_function(name, |b| {
            b.iter(|| black_box(&packed).view_contains(black_box(&v), 2, 123_457))
        });
    }
    g.finish();
}

/// Direct blocked intersection counts against the public select-and-count
/// oracle that the Flight fallback used before the grouped scalar arm.
fn intersection_cardinalities(c: &mut Criterion) {
    const COUNT_SETS: u32 = 512;
    const COUNT_STRIDE: u64 = 4_096;
    let view = View::blocked(COUNT_SETS, COUNT_STRIDE);
    let mut packed = OrdSet::from_iter_unsorted((0..COUNT_SETS as u64).flat_map(|owner| {
        (0..COUNT_STRIDE)
            .filter(move |x| (x + owner) % 2 == 0)
            .map(move |x| owner * COUNT_STRIDE + x)
    }));
    packed.optimize();
    assert_has_bitmaps(&packed, "intersection cardinalities");
    let filter = OrdSet::from_iter_unsorted(
        (0..32).map(|i| ((i * 127 + 11) % COUNT_STRIDE as usize) as u64),
    );

    let mut g = c.benchmark_group("view/intersection_cardinalities");
    g.bench_function("blocked_bitmap/arm", |b| {
        b.iter(|| {
            black_box(&packed).view_intersection_cardinalities(black_box(&view), black_box(&filter))
        })
    });
    g.bench_function("blocked_bitmap/via_select", |b| {
        b.iter(|| {
            (0..COUNT_SETS)
                .map(|row| packed.view_select(&view, row).and_cardinality(&filter))
                .collect::<Vec<_>>()
        })
    });
    g.finish();
}

/// The interleaved grouped walk against the select-and-combine oracle.
///
/// Both sides are shipped code: the oracle below is the same sequence of public
/// calls `fold_via_select` makes, so the ratio is free of the bench-local bias
/// `setops.rs` records.
fn fold(c: &mut Criterion) {
    let parts = constituents();
    let v = View::interleaved(SETS);
    let packed = pack(v, &parts);
    assert_has_bitmaps(&packed, "fold");

    let mut g = c.benchmark_group("view/fold");
    for (name, r) in [
        ("any/arm", Reduce::Any),
        ("all/arm", Reduce::All),
        ("parity/arm", Reduce::Parity),
    ] {
        g.bench_function(name, |b| {
            b.iter(|| black_box(&packed).view_fold(black_box(&v), r))
        });
    }
    g.bench_function("any/via_select", |b| {
        b.iter(|| {
            let mut acc = black_box(&packed).view_select(&v, 0);
            for i in 1..SETS {
                acc = acc.or(&packed.view_select(&v, i));
            }
            black_box(acc)
        })
    });
    g.finish();
}

/// The interval arm against building the same answer one ordinal at a time.
fn expand(c: &mut Criterion) {
    let v = View::interleaved(SETS);
    let mut src = OrdSet::from_iter_unsorted(0..LOGICAL);
    src.optimize();
    let mut sparse = OrdSet::from_iter_unsorted((0..LOGICAL).filter(|x| x.is_multiple_of(7)));
    sparse.optimize();

    let mut g = c.benchmark_group("view/expand");
    g.bench_function("contiguous/arm", |b| {
        b.iter(|| black_box(&src).view_expand(black_box(&v)))
    });
    g.bench_function("sparse/arm", |b| {
        b.iter(|| black_box(&sparse).view_expand(black_box(&v)))
    });
    // The alternative: every slot as its own ordinal, which is what a caller
    // without the interval construction would write.
    //
    // **Both inputs get this row, and the first version of this file gave it
    // only to the contiguous one.** That made `sparse/arm` unjudgeable — it is
    // slower than `contiguous/arm` in absolute terms, which looks like a defect
    // until the sparse baseline is there to compare it to. An arm measured
    // against no alternative is not measured.
    for (name, s) in [("contiguous", &src), ("sparse", &sparse)] {
        g.bench_function(format!("{name}/per_ordinal"), |b| {
            b.iter(|| {
                let mut out: Vec<u64> = Vec::new();
                for x in black_box(s).iter() {
                    for i in 0..SETS {
                        if let Some(o) = v.ordinal_of(i, x) {
                            out.push(o);
                        }
                    }
                }
                let mut t = OrdSet::from_iter_unsorted(out);
                t.optimize();
                black_box(t)
            })
        });
    }
    g.finish();
}

/// Packing constituents in. `ViewSink` accumulates ordinals, so this row is
/// the one that would move if the aligned refcount arm were ever built.
fn build(c: &mut Criterion) {
    let parts = constituents();
    let mut g = c.benchmark_group("view/build");
    for (name, v) in [
        ("interleaved", View::interleaved(SETS)),
        ("blocked_aligned", View::blocked(SETS, ALIGNED)),
    ] {
        g.bench_function(name, |b| b.iter(|| black_box(pack(v, &parts))));
    }
    g.finish();
}

criterion_group!(
    benches,
    select,
    cardinality,
    intersection_cardinalities,
    fold,
    expand,
    build
);
criterion_main!(benches);
