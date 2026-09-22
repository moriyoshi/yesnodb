//! Deliberate handoff points to Apache Arrow. **Semver-exempt.**
//!
//! # Stability
//!
//! This module names types from `arrow-buffer`. Its API therefore tracks
//! arrow-buffer's major version and is **exempt from this crate's semver
//! guarantees**: breaking changes here may land in a minor release. Everything
//! else in `yesno-core` keeps Arrow out of its signatures, so an arrow-buffer
//! major bump is a *patch* release of the crate rather than a breaking one.
//! ( The same pattern `object_store` and `sqlx` use for their own leaked
//! dependencies. )
//!
//! # The selection mask is the point
//!
//! `BooleanBuffer` is LSB-first bit-packed little-endian, which is **bit
//! identical** to the Roaring bitmap container layout. A bitmap container
//! therefore *is* an Arrow selection mask over 65 536 rows, and handing it to a
//! filter kernel costs one refcount bump.
//!
//! That matters because a posting list applied as a row filter never needs
//! materializing as integers at all. Expanding a dense chunk to 65 536 `u64`s to
//! then filter with it would cost 512 KiB and a decode loop; [`bitmap_mask`]
//! costs an atomic increment on a store-backed payload, or an 8 KiB re-encode on
//! a memtable-resident one. Either way it is the 512 KiB that is avoided, which
//! is the point — but the two cases are not the same cost, and the difference
//! is spelled out on [`bitmap_mask`] itself.

use arrow_buffer::{BooleanBuffer, Buffer};

use crate::container::Container;
use crate::CHUNK_CARD;

/// Re-exported so downstream crates provably link the same `arrow-buffer`.
pub use arrow_buffer;

/// The container's bits as an Arrow mask, if it is a bitmap.
///
/// `None` for array and run containers, which have no bit image to lend; use
/// [`container_mask`] when any container will do and a build is acceptable.
///
/// # Zero-copy for one of the two payload kinds, not both
///
/// A **store-backed** payload is 8 192 mmap'd bytes already behind a `Buffer`,
/// and is handed over as a refcount bump. A **memtable-resident** payload is a
/// `Vec<u64>` with no `Buffer` behind it, so one is built: all 8 KiB are
/// re-encoded word by word into little-endian bytes.
///
/// Both are far cheaper than expanding the chunk to integers, which is the
/// comparison that matters and the reason to reach for this. But they are not
/// the same cost, and a hot loop over freshly written data pays the second one
/// every call. This documentation claimed O(1) unconditionally until
/// 2026-09-13.
#[inline]
pub fn bitmap_mask(c: &Container) -> Option<BooleanBuffer> {
    match c {
        Container::Bitmap(b) => Some(b.bits().to_boolean_buffer()),
        _ => None,
    }
}

/// The container's 1024 payload words, borrowed, if they can be lent.
///
/// This is the read-only counterpart to [`bitmap_mask`]: it never copies, and
/// says so by refusing instead. A caller that would rather take another path
/// than pay an 8 KiB re-encode can ask here first and fall back to
/// [`container_mask`] only when it has to.
///
/// # `None` has two classes of cause, and they are not the same
///
/// * **Not a bitmap.** Array and run containers store positions, not bits, so
///   there is no word image to lend — the same condition [`bitmap_mask`]
///   reports.
/// * **A bitmap whose payload cannot be viewed as native `u64` words.** A
///   store-backed payload may be at an unaligned offset, and its little-endian
///   bytes cannot be borrowed as native words on a big-endian host.
///   `BooleanBuffer` itself has neither restriction.
///
/// **Do not read `None` as "not a bitmap".** A caller that branches on kind and
/// then unwraps here will panic on a healthy shared container whose alignment
/// or byte order prevents borrowing. Treat it as "no borrow available" and
/// take the decoding or generic path.
#[inline]
pub fn bitmap_words(c: &Container) -> Option<&[u64]> {
    match c {
        Container::Bitmap(b) => b.bits().try_words(),
        _ => None,
    }
}

/// The container's bits as an Arrow mask of exactly [`CHUNK_CARD`] bits.
///
/// For a bitmap this defers to [`bitmap_mask`], which is a refcount bump on a
/// store-backed payload and an 8 KiB re-encode on a memtable-resident one. For
/// an array or run this builds the image, which is unavoidable — they store
/// positions, not bits.
pub fn container_mask(c: &Container) -> BooleanBuffer {
    if let Some(m) = bitmap_mask(c) {
        return m;
    }
    let mut words = vec![0u64; crate::BITMAP_WORDS];
    for v in c.iter() {
        words[(v >> 6) as usize] |= 1u64 << (v & 63);
    }
    let mut bytes = Vec::with_capacity(crate::BITMAP_BYTES);
    for w in &words {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    BooleanBuffer::new(Buffer::from_vec(bytes), 0, CHUNK_CARD as usize)
}

/// One process-wide zeroed chunk, so an absent prefix costs a refcount bump.
///
/// `BooleanBuffer::new_unset` allocates and zeroes 8 KiB **per call**, and
/// the call site is per absent chunk: a mask stream walking a contiguous ordinal
/// space over a sparse set is mostly gaps, so that is 8 KiB of allocate-and-memset
/// for every hole. The design says gaps should be "literally free" and names the
/// mechanism — one shared zero buffer — which is this.
fn zero_chunk() -> &'static Buffer {
    static ZERO: std::sync::OnceLock<Buffer> = std::sync::OnceLock::new();
    ZERO.get_or_init(|| Buffer::from_vec(vec![0u8; crate::BITMAP_BYTES]))
}

/// An all-zero mask, for a chunk absent from a stream.
///
/// A consumer scanning a contiguous ordinal space needs a mask per chunk even
/// where the set has nothing, and this is the cheap way to say "nothing here" —
/// **free**, not merely cheap: every gap shares `zero_chunk` and pays a
/// refcount bump. Do not replace this with `BooleanBuffer::new_unset`, which
/// looks equivalent and allocates 8 KiB each time.
#[inline]
pub fn empty_mask() -> BooleanBuffer {
    BooleanBuffer::new(zero_chunk().clone(), 0, CHUNK_CARD as usize)
}

#[cfg(test)]
mod tests {

    /// A lent word slice must be the container's actual bits, and must agree
    /// with the mask built from the same payload.
    #[test]
    fn lent_words_are_the_same_bits_the_mask_reports() {
        let vals: Vec<u16> = [0u16, 1, 63, 64, 4096, 4097, 65535].to_vec();
        let c = Container::Bitmap(crate::container::BitmapContainer::from_sorted(&vals));

        let words = bitmap_words(&c).expect("a memtable-resident bitmap lends its words");
        assert_eq!(words.len(), crate::BITMAP_WORDS);

        // Against the container itself, and against the Arrow mask, which is
        // built by a different route through `to_boolean_buffer`.
        let mask = bitmap_mask(&c).unwrap();
        for v in 0..crate::CHUNK_CARD {
            let in_words = words[(v >> 6) as usize] >> (v & 63) & 1 == 1;
            assert_eq!(in_words, c.contains(v as u16), "word bit {v}");
            assert_eq!(in_words, mask.value(v as usize), "mask bit {v}");
        }
    }

    /// Array and run containers store positions, so there is nothing to lend.
    #[test]
    fn a_container_with_no_bit_image_lends_nothing() {
        assert!(bitmap_words(&Container::from_sorted(&[1, 2, 3])).is_none());
        assert!(
            bitmap_words(&Container::Run(crate::container::RunContainer::from_pairs(
                &[(0, 9)]
            )))
            .is_none()
        );
    }

    /// The two `None` causes must not be conflated.
    ///
    /// `bitmap_mask` refuses only non-bitmaps; `bitmap_words` refuses those
    /// *and* a bitmap it cannot borrow as `u64`. So a lent slice implies a mask,
    /// but a mask does not imply a lent slice — and a caller that reads `None`
    /// as "not a bitmap" is reading the wrong implication.
    #[test]
    fn lending_words_implies_a_mask_but_not_the_other_way_round() {
        let c = Container::Bitmap(crate::container::BitmapContainer::from_sorted(&[7u16]));
        assert!(bitmap_words(&c).is_some() && bitmap_mask(&c).is_some());

        for c in [
            Container::from_sorted(&[1, 2, 3]),
            Container::Run(crate::container::RunContainer::from_pairs(&[(0, 9)])),
        ] {
            assert_eq!(bitmap_words(&c).is_none(), bitmap_mask(&c).is_none());
        }
    }
    use super::*;
    use crate::container::BitmapContainer;

    #[test]
    fn a_bitmap_container_is_already_a_mask() {
        let vals: Vec<u16> = (0..5000u16).map(|i| i * 3).collect();
        let c = Container::Bitmap(BitmapContainer::from_sorted(&vals));
        let m = bitmap_mask(&c).expect("a bitmap must lend its bits");
        assert_eq!(m.len(), CHUNK_CARD as usize);
        assert_eq!(m.count_set_bits(), vals.len());
        for &v in &vals {
            assert!(m.value(v as usize), "bit {v} should be set");
        }
        assert!(!m.value(1), "3 does not divide 1");
    }

    #[test]
    fn the_bitmap_path_does_not_copy() {
        // The whole justification for the Roaring/Arrow bit-order alignment: the
        // mask must share the container's allocation, not duplicate it.
        let mut c = Container::Bitmap(BitmapContainer::from_sorted(
            &(0..5000u16).map(|i| i * 3).collect::<Vec<_>>(),
        ));
        c.freeze();
        let m = bitmap_mask(&c).unwrap();
        let bits_ptr = m.values().as_ptr();
        let m2 = bitmap_mask(&c).unwrap();
        assert_eq!(
            bits_ptr,
            m2.values().as_ptr(),
            "two masks from one frozen container must share bytes"
        );
    }

    #[test]
    fn arrays_and_runs_have_no_bits_to_lend() {
        assert!(bitmap_mask(&Container::from_sorted(&[1, 2, 3])).is_none());
        assert!(
            bitmap_mask(&Container::Run(crate::container::RunContainer::from_pairs(
                &[(0, 10)]
            )))
            .is_none()
        );
    }

    #[test]
    fn container_mask_agrees_across_every_kind() {
        // The same set encoded three ways must produce the same mask.
        let vals: Vec<u16> = (0..300u16).map(|i| i * 7).collect();
        let arr = Container::from_sorted(&vals);
        let bm = Container::Bitmap(BitmapContainer::from_sorted(&vals));
        let rn = Container::Run(crate::container::RunContainer::from_sorted_values(
            vals.iter().copied(),
        ));

        let masks = [
            container_mask(&arr),
            container_mask(&bm),
            container_mask(&rn),
        ];
        for m in &masks {
            assert_eq!(m.len(), CHUNK_CARD as usize);
            assert_eq!(m.count_set_bits(), vals.len());
        }
        // Bit for bit, not merely equal in cardinality.
        assert_eq!(masks[0].values(), masks[1].values());
        assert_eq!(masks[1].values(), masks[2].values());
    }

    #[test]
    fn mask_bit_order_matches_the_roaring_layout() {
        // If this ever fails, the zero-copy handoff is unsound and every mask
        // must be rebuilt instead.
        let vals = [0u16, 1, 63, 64, 65, 4095, 65535];
        let c = Container::Bitmap(BitmapContainer::from_sorted(&vals));
        let m = bitmap_mask(&c).unwrap();
        for &v in &vals {
            assert!(
                m.value(v as usize),
                "Arrow disagrees with Roaring at bit {v}"
            );
        }
        assert_eq!(m.count_set_bits(), vals.len());
    }

    #[test]
    fn an_empty_mask_selects_nothing() {
        let m = empty_mask();
        assert_eq!(m.len(), CHUNK_CARD as usize);
        assert_eq!(m.count_set_bits(), 0);
    }

    #[test]
    fn a_full_container_masks_everything() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(0, 65535);
        let m = bitmap_mask(&Container::Bitmap(b)).unwrap();
        assert_eq!(m.count_set_bits(), CHUNK_CARD as usize);
    }
}
