//! The whole-file import path: `roaring_format::deserialize_u64` over arbitrary
//! bytes.
//!
//! This is the *outer* boundary — `decode_container` fuzzes one payload, this
//! fuzzes the header parsing, container directory, and offset handling around
//! them. Two things here have a history of being subtly wrong in Roaring
//! implementations generally, so they are worth the separate target:
//!
//!   - the u32 offset header is present iff `cookie == 0x00003B4B`, **or**
//!     ( the run cookie is used AND `n_keys >= 4` ). Getting that condition
//!     wrong silently misparses small run-encoded bitmaps rather than failing;
//!   - a container directory can name offsets that overlap, run backwards, or
//!     point outside the file.
//!
//! The property is the same as everywhere else in this crate: `Err` or a valid
//! result, never a panic.

#![no_main]

use libfuzzer_sys::fuzz_target;
use yesno_core::roaring_format::{deserialize_u64, serialize_u64};

fuzz_target!(|data: &[u8]| {
    let Ok(s) = deserialize_u64(data) else {
        return;
    };

    // Cardinality must agree with what is actually iterable. `len()` is O(1) and
    // cached per container, so it is a genuinely independent claim from the
    // contents rather than a restatement of them.
    let mut count = 0u64;
    let mut prev: Option<u64> = None;
    for v in s.iter() {
        // A set iterates in strictly ascending order, by definition.
        if let Some(p) = prev {
            assert!(v > p, "ordinals not strictly ascending: {p} then {v}");
        }
        prev = Some(v);
        count += 1;
    }
    assert_eq!(
        count,
        s.len(),
        "cached cardinality disagrees with iteration"
    );

    // Re-serializing must be a fixed point. This is the property that makes
    // `O(container count)` import of a foreign file legitimate: if our decode
    // and encode did not agree, byte-identity with the portable format would be
    // an accident rather than a guarantee.
    let bytes = serialize_u64(&s);
    let round = deserialize_u64(&bytes).expect("our own serialization must re-read");
    assert_eq!(
        round.len(),
        s.len(),
        "cardinality changed across a round trip"
    );
    assert!(
        round.iter().eq(s.iter()),
        "contents changed across a serialize/deserialize round trip"
    );
});
