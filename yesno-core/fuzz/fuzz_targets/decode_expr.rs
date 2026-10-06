//! The wire expression decoders: `SetExpr::decode`, `AnyExpr::decode` and
//! `QueryRequest::decode` over arbitrary bytes.
//!
//! These are fuzz targets **by contract**, for the same reason
//! `container::codec::decode` is: for any input they must return `Err` or a
//! value satisfying their invariants, and must never panic. This surface is now
//! the larger of the two. A container payload arrives inside a file the server
//! wrote; an expression arrives from a *client*, through a Flight ticket or a
//! `QueryRequest` descriptor command, and nothing upstream of the decoder has
//! looked at it.
//!
//! What the decoder promises, and therefore what this target asserts:
//!
//!   - **the admission bounds hold on whatever it accepts**. `MAX_DEPTH` and
//!     `MAX_NODES` bound the tree, `MAX_VALUE_BITS` the width of a `Big` value,
//!     `MAX_RESULT_BITS` the arity-times-width product a `Vec[Big]` denotes, and
//!     `MAX_WORK` the limb operations either can ask for. All five are checked
//!     at decode time precisely so that a short payload cannot denote an
//!     expensive query, so a payload that decodes with a bound violated is the
//!     amplification the caps exist to refuse;
//!   - **decoding is a fixed point**. Re-encoding an accepted expression and
//!     decoding it again must yield the same value. The wire format is
//!     canonical by construction -- `NonCanonicalLiteral`, `NonCanonicalView`
//!     and `NonCanonicalBigLit` are refusals, not normalizations -- so this is a
//!     second, independent statement of what the first decode decided;
//!   - **the two entry points agree on a set**. `SetExpr::decode` is the legacy
//!     single-sort reader and `AnyExpr::decode` the multi-sorted one; they share
//!     a header and a tag table, and a payload the first accepts must read as
//!     `AnyExpr::Set` of the same expression in the second. A server picks
//!     between them by `looks_like_expr`, so a disagreement here is a payload
//!     that means one thing on one code path and another on the other;
//!   - **an accepted expression re-encodes to something the dispatch still
//!     recognises**. `looks_like_expr` is what separates an expression from a
//!     bare 8-byte key on a descriptor, and an expression encoding to exactly
//!     eight bytes would be silently read as a key instead. `encode` carries a
//!     `debug_assert` for that; this reaches it with shapes no unit test
//!     enumerates;
//!   - **`keys` is bounded and appends**. A router calls it before evaluating
//!     anything, so it must not be its own amplification: every walk pushes at
//!     most one key per node, and the node cap therefore bounds the result.
//!
//! What it deliberately does not assert is *how* a malformed payload is
//! refused. The contract is "`Err` or valid", not a particular error: the sorts
//! reject each other's tags, but which of `SortMismatch`, `UnknownTag` and
//! `Truncated` a given mutation lands on is an implementation detail, and
//! pinning it here would make the target fail on a legitimate improvement to
//! the diagnostics.

#![no_main]

use libfuzzer_sys::fuzz_target;
use yesno_wire::{
    AnyExpr, QueryRequest, SetExpr, MAX_NODES, MAX_RESULT_BITS, MAX_VALUE_BITS, MAX_WORK,
};

/// The invariants an accepted expression must satisfy, whichever entry point
/// produced it.
fn check(e: &AnyExpr) {
    // The admission bounds. Only the ones the decoder actually enforces: a
    // `Vec[Big]` element is *not* capped at `MAX_VALUE_BITS` -- a zip of two
    // 2^20-bit vectors denotes 2^21-bit elements and is legitimately accepted,
    // because what bounds that node is the product against `MAX_RESULT_BITS`.
    match e {
        AnyExpr::Big(b) => {
            assert!(
                b.width_bound() <= MAX_VALUE_BITS,
                "decoded a Big wider than MAX_VALUE_BITS: {} > {MAX_VALUE_BITS}",
                b.width_bound()
            );
            assert!(
                b.work_bound() <= MAX_WORK,
                "decoded a Big costing more than MAX_WORK: {} > {MAX_WORK}",
                b.work_bound()
            );
        }
        AnyExpr::VecBig(v) => {
            assert!(
                v.result_bound() <= MAX_RESULT_BITS,
                "decoded a Vec[Big] result wider than MAX_RESULT_BITS: {} > {MAX_RESULT_BITS}",
                v.result_bound()
            );
            assert!(
                v.work_bound() <= MAX_WORK,
                "decoded a Vec[Big] costing more than MAX_WORK: {} > {MAX_WORK}",
                v.work_bound()
            );
        }
        AnyExpr::Set(_) | AnyExpr::VecInt(_) => {}
    }

    // `keys` is an out-parameter that appends. Seeding it proves that, and
    // reading the tail back keeps the walk from being optimised away.
    const SENTINEL: u64 = 0xDEAD_BEEF_DEAD_BEEF;
    let mut keys = vec![SENTINEL];
    e.keys(&mut keys);
    assert_eq!(
        keys[0], SENTINEL,
        "keys cleared its output instead of appending"
    );
    assert!(
        keys.len() - 1 <= MAX_NODES,
        "keys named {} keys, more than the {MAX_NODES}-node cap allows",
        keys.len() - 1
    );

    // Re-encoding and decoding must be a fixed point.
    let bytes = e.encode();
    assert!(
        SetExpr::looks_like_expr(&bytes),
        "an accepted expression re-encodes to {} bytes that the descriptor \
         dispatch would not read as an expression",
        bytes.len()
    );
    match AnyExpr::decode(&bytes) {
        Ok(back) => assert_eq!(&back, e, "re-decoding an encoded expression changed it"),
        Err(err) => panic!("an expression we encoded failed to re-decode: {err:?}"),
    }
}

fuzz_target!(|data: &[u8]| {
    // The legacy single-sort reader first, so its result can be compared with
    // the multi-sorted one on the same bytes.
    let set = SetExpr::decode(data).ok();

    match AnyExpr::decode(data) {
        Ok(any) => {
            check(&any);
            if let Some(s) = set {
                assert_eq!(
                    any,
                    AnyExpr::Set(s),
                    "the two decoders disagree about the same payload"
                );
            }
        }
        Err(_) => assert!(
            set.is_none(),
            "SetExpr::decode accepted a payload AnyExpr::decode refuses"
        ),
    }

    // The outer envelope: its own magic, its own version byte, a flag field and
    // a pinned-version field whose meaning depends on that flag, wrapped around
    // the payload above.
    if let Ok(q) = QueryRequest::decode(data) {
        check(&q.expression);
        let bytes = q.encode();
        assert!(
            QueryRequest::looks_like_request(&bytes),
            "an accepted request re-encodes to bytes the dispatch would not read as one"
        );
        match QueryRequest::decode(&bytes) {
            Ok(back) => assert_eq!(back, q, "re-decoding an encoded request changed it"),
            Err(err) => panic!("a request we encoded failed to re-decode: {err:?}"),
        }
    }
});
