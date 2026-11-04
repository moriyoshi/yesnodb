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
use yesno_core::CodecError;

/// How many times a truncated input is padded and retried.
const FIT_ROUNDS: usize = 8;
/// The most bytes a fitted input may reach: eight bitmap payloads.
const FIT_MAX_LEN: usize = 8 * yesno_core::BITMAP_BYTES;

fuzz_target!(|data: &[u8]| {
    check(data);

    // A bitmap container is 8192 bytes and libFuzzer's default `-max_len` is
    // 4096, so a bitmap *read from the input* was never reached: on
    // 2026-10-25 no corpus input grew past 1132 bytes even at
    // `-max_len=16384`. The parser ignores trailing bytes and reports a short
    // read as `Truncated { expected, found }`, so a truncated input is
    // retried padded by exactly the shortfall, a few rounds, up to
    // `FIT_MAX_LEN`. The unpadded input is always checked first, so
    // truncation itself is still fuzzed, and an input that parses is never
    // padded.
    //
    // The fill is the input's last byte, not zero. This format reads a bitmap
    // only when the declared cardinality is over 4096, so its payload needs
    // over 4096 set bits matching the declaration exactly. With a zero fill
    // the fuzzer would have to write at least 513 bytes of them, and 300 s of
    // it produced 0 valid input bitmaps. A `0xFF` fill under a declared 65536
    // is a valid full bitmap, and an input ending in `0x00` is zero-padded.
    let fill = data.last().copied().unwrap_or(0);
    let mut fitted = data.to_vec();
    for _ in 0..FIT_ROUNDS {
        let Err(CodecError::Truncated { expected, found }) = deserialize_u64(&fitted) else {
            return;
        };
        let grow = expected.saturating_sub(found).max(1);
        if fitted.len().saturating_add(grow) > FIT_MAX_LEN {
            return;
        }
        fitted.resize(fitted.len() + grow, fill);
        check(&fitted);
    }
});

/// `Err` or a valid set, never a panic.
fn check(data: &[u8]) {
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
}
