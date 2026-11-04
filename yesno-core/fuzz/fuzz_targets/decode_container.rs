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
use yesno_core::unstable_arrow::arrow_buffer::Buffer;
use yesno_core::{CodecError, Container, ContainerKind};

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
    // The high bit of the selector asks for a bitmap payload fitted to
    // `BITMAP_BYTES`, zero-padded or truncated. Without it no bitmap could ever
    // decode: libFuzzer's default `-max_len` is 4096 and a bitmap payload is
    // exactly 8192 bytes, so before this every bitmap input in every run stopped
    // at the length check, and the popcount cross-check, `validate` on a bitmap
    // and bitmap re-encoding were never reached (measured 2026-10-25 by
    // `cargo fuzz coverage`). Fitting only bitmaps is deliberate: an array's
    // or a run's length follows from a value inside the input, so padding them
    // makes nothing reachable that a short input cannot already express. With
    // the bit clear the payload is passed unchanged, so wrong lengths are still
    // fuzzed.
    let fitted;
    let payload = if kind == ContainerKind::Bitmap && data[0] & 0x80 != 0 {
        let mut p = data[5..].to_vec();
        p.resize(yesno_core::BITMAP_BYTES, 0);
        fitted = p;
        &fitted[..]
    } else {
        &data[5..]
    };

    let decoded = codec::decode(kind, payload, card);
    agrees_with_the_mmap_path(kind, payload, card, &decoded);

    if let Ok(c) = decoded {
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

        // And in bytes, which the contents check cannot see: a decoder that
        // accepted a second spelling and normalized it, say an array payload
        // with a byte to spare, would round-trip its contents perfectly. An
        // array or bitmap payload is the only spelling of its contents, so it
        // must come back exactly. A run payload comes back with touching runs
        // merged, which `decode_run` does on purpose for foreign files, so it
        // is compared with a coalescing written here. Its declared `card` is
        // ignored by design and is not part of the payload. Measured
        // 2026-10-25 over the corpora: 100 identical, 23 coalesced runs, 0
        // other.
        assert_eq!(c.kind(), kind, "decode changed the container kind");
        let canonical = if kind == ContainerKind::Run {
            coalesced(payload)
        } else {
            payload.to_vec()
        };
        assert!(
            re == canonical,
            "an accepted {kind:?} payload is not the canonical spelling of its contents"
        );

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

/// `payload`, an accepted run payload, with every pair of touching runs merged.
fn coalesced(payload: &[u8]) -> Vec<u8> {
    let u16_at = |at: usize| u16::from_le_bytes([payload[at], payload[at + 1]]) as u32;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for i in 0..u16_at(0) as usize {
        let start = u16_at(2 + 4 * i);
        let end = start + u16_at(4 + 4 * i);
        match runs.last_mut() {
            Some((_, prev_end)) if start == *prev_end + 1 => *prev_end = end,
            _ => runs.push((start, end)),
        }
    }
    let mut out = (runs.len() as u16).to_le_bytes().to_vec();
    for (start, end) in runs {
        out.extend_from_slice(&(start as u16).to_le_bytes());
        out.extend_from_slice(&((end - start) as u16).to_le_bytes());
    }
    out
}

/// `codec::decode_buffer`, the mmap path, must agree with `codec::decode` on
/// what `decode` accepts, at every alignment.
///
/// The two do not share a contract. `decode_buffer` reads pages whose CRC has
/// been verified and deliberately skips an array's ordering and a bitmap's
/// popcount, so where `decode` refuses one of those it may accept, and that is
/// not asserted. Its run arm is different: to keep the pairs shared it repeats
/// every check `decode_run` makes, in its own loop, so on runs the two must
/// reach the same verdict on *every* input, not just the accepted ones. Three
/// placements cover each path: an address that is 0 mod 8 shares everything,
/// 2 mod 8 shares `u16` data but not a bitmap, and 1 mod 8 shares nothing.
fn agrees_with_the_mmap_path(
    kind: ContainerKind,
    payload: &[u8],
    card: u32,
    decoded: &Result<Container, CodecError>,
) {
    for residue in [0usize, 2, 1] {
        let mut bytes = vec![0u8; payload.len() + 8];
        let off = (residue + 8 - bytes.as_ptr() as usize % 8) % 8;
        bytes[off..off + payload.len()].copy_from_slice(payload);
        // Taking the `Vec` does not copy it, but the alignment reported is
        // read back from the buffer rather than assumed.
        let buf = Buffer::from(bytes);
        let at = (buf.as_ptr() as usize + off) % 8;
        let shared = codec::decode_buffer(kind, &buf, off, payload.len(), card);
        match (decoded, &shared) {
            (Ok(a), Ok(b)) => {
                assert_eq!(
                    a.kind(),
                    b.kind(),
                    "decode_buffer changed the kind, at {at} mod 8"
                );
                assert_eq!(
                    a.len(),
                    b.len(),
                    "decode_buffer changed the cardinality, at {at} mod 8"
                );
                assert!(
                    a.iter().eq(b.iter()),
                    "decode_buffer changed the contents, at {at} mod 8"
                );
                // The same contents in touching runs left unmerged would pass
                // the line above and break every run kernel.
                assert!(
                    codec::encode(a) == codec::encode(b),
                    "decode_buffer changed the representation, at {at} mod 8"
                );
            }
            (Ok(_), Err(e)) => {
                panic!(
                    "decode accepted a {kind:?} payload decode_buffer refuses at {at} mod 8: {e:?}"
                )
            }
            (Err(_), Ok(_)) if kind == ContainerKind::Run => {
                panic!("decode_buffer accepted a run payload decode refuses, at {at} mod 8")
            }
            (Err(_), _) => {}
        }
    }
}
