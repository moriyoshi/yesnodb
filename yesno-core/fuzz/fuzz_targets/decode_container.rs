//! `container::codec::decode` is a fuzz target **by contract**: for any input it
//! must return `Err` or a container satisfying its invariants, and must never
//! panic. `AGENTS.md` states that rule; this target is what enforces it.
//!
//! `codec::validate` is the definition of "satisfying its invariants", so the
//! whole property is: whatever `decode` returns `Ok` for, `validate` must accept.
//!
//! This is not hypothetical coverage. When the target was first written, four
//! inputs satisfying every structural check still decoded to `Ok`:
//!
//!   - an array whose values descend, breaking every binary search over it;
//!   - a bitmap whose stated cardinality is not its popcount, silently
//!     corrupting every identity in `ops::card`;
//!   - runs that overlap, so iteration yields the same ordinal twice;
//!   - a run reaching past the end of the chunk, which *panicked* on first
//!     access because `RunContainer::end` adds `start + len_minus_1` in `u16`.
//!
//! All four are fixed and pinned by unit tests in `codec.rs` plus properties in
//! `proptest_oracle.rs`. They are recorded here because they are the shape of
//! defect this target exists to find: **structurally valid, semantically
//! impossible**. A future regression will look like them, not like a truncated
//! buffer — length errors were always caught.

#![no_main]

use libfuzzer_sys::fuzz_target;
use yesno_core::container::codec;
use yesno_core::ContainerKind;

fuzz_target!(|data: &[u8]| {
    // Need a kind selector and a cardinality before the payload begins.
    if data.len() < 5 {
        return;
    }
    let kind = match data[0] % 3 {
        0 => ContainerKind::Array,
        1 => ContainerKind::Bitmap,
        _ => ContainerKind::Run,
    };
    // Take the cardinality from the input rather than deriving it from the
    // payload. It arrives from the file in production too, so letting the
    // fuzzer disagree with the payload is the point, not a nuisance — that
    // disagreement is exactly the bitmap-popcount defect.
    let card = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    let payload = &data[5..];

    if let Ok(c) = codec::decode(kind, payload, card) {
        assert!(
            codec::validate(&c).is_ok(),
            "decode returned a container that validate rejects: kind={kind:?} card={card} \
             payload_len={}",
            payload.len()
        );

        // Touch the contents. `validate` walks the structure, but the u16
        // overflow above only fired when something actually read `end`, so a
        // target that stops at `validate` would have missed the one defect here
        // that was a panic rather than a bad value.
        let n = c.iter().count();
        assert_eq!(
            n as u32,
            c.len(),
            "cached cardinality disagrees with iteration"
        );

        // Round-tripping is a second, independent check on the same container:
        // re-encoding and decoding must be a fixed point.
        let re = codec::encode(&c);
        match codec::decode(c.kind(), &re, c.len()) {
            Ok(back) => {
                assert_eq!(
                    back.iter().collect::<Vec<u16>>(),
                    c.iter().collect::<Vec<u16>>(),
                    "re-decoding an encoded container changed its contents"
                );
            }
            Err(e) => panic!("a container we produced failed to re-decode: {e:?}"),
        }
    }
});
