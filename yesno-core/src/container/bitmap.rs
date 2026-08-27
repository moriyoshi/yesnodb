//! A 65536-bit dense container with an exact cached cardinality.
//!
//! The cached `len` is what makes the cardinality identities cheap:
//! `|A ∪ B| = |A| + |B| − |A ∩ B|` and friends need `len()` to be O(1), so every
//! mutation maintains it incrementally rather than recounting.

use std::borrow::Cow;

use crate::buffer::BitStore;
use crate::{BITMAP_WORDS, CHUNK_CARD};

/// Invariant: `len == popcount(words)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BitmapContainer {
    pub(crate) bits: BitStore,
    pub(crate) len: u32,
}

#[inline]
const fn word_of(v: u16) -> usize {
    (v >> 6) as usize
}

#[inline]
const fn mask_of(v: u16) -> u64 {
    1u64 << (v & 63)
}

impl BitmapContainer {
    pub fn zeroed() -> Self {
        BitmapContainer {
            bits: BitStore::zeroed(),
            len: 0,
        }
    }

    pub fn from_words(words: Vec<u64>, len: u32) -> Self {
        debug_assert_eq!(words.len(), BITMAP_WORDS);
        debug_assert_eq!(
            words.iter().map(|w| w.count_ones()).sum::<u32>(),
            len,
            "cached len must equal popcount"
        );
        BitmapContainer {
            bits: BitStore::from_words(words),
            len,
        }
    }

    pub(crate) fn from_store(bits: BitStore, len: u32) -> Self {
        BitmapContainer { bits, len }
    }

    /// Build from sorted unique values (used by array -> bitmap promotion).
    pub fn from_sorted(vals: &[u16]) -> Self {
        let mut w = vec![0u64; BITMAP_WORDS];
        for &v in vals {
            w[word_of(v)] |= mask_of(v);
        }
        let len = w.iter().map(|x| x.count_ones()).sum();
        BitmapContainer {
            bits: BitStore::from_words(w),
            len,
        }
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// The underlying bit store, for the deliberate Arrow handoff in
    /// `crate::unstable_arrow`. Crate-internal so the Arrow type does not leak
    /// into this crate's stable surface ( policy R1 ).
    #[inline]
    pub(crate) fn bits(&self) -> &crate::buffer::BitStore {
        &self.bits
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len == CHUNK_CARD
    }

    #[inline]
    pub fn contains(&self, v: u16) -> bool {
        match self.bits.try_words() {
            Some(w) => w[word_of(v)] & mask_of(v) != 0,
            None => self.contains_slow(v),
        }
    }

    #[cold]
    fn contains_slow(&self, v: u16) -> bool {
        // Unaligned shared buffer: read the one word we need, byte-wise.
        let bytes = self.bits.to_le_bytes();
        let i = word_of(v) * 8;
        let w = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        w & mask_of(v) != 0
    }

    /// Returns whether the bit changed, so `len` stays exact in O(1).
    pub fn insert(&mut self, v: u16) -> bool {
        let w = self.bits.words_mut();
        let (i, m) = (word_of(v), mask_of(v));
        let was = w[i] & m != 0;
        w[i] |= m;
        self.len += !was as u32;
        !was
    }

    pub fn remove(&mut self, v: u16) -> bool {
        let w = self.bits.words_mut();
        let (i, m) = (word_of(v), mask_of(v));
        let was = w[i] & m != 0;
        w[i] &= !m;
        self.len -= was as u32;
        was
    }

    /// Flip one bit; returns the new state.
    pub fn toggle(&mut self, v: u16) -> bool {
        let w = self.bits.words_mut();
        let (i, m) = (word_of(v), mask_of(v));
        let was = w[i] & m != 0;
        w[i] ^= m;
        if was {
            self.len -= 1;
        } else {
            self.len += 1;
        }
        !was
    }

    /// Set `[start, end]` inclusive. Returns how many bits actually changed.
    pub fn insert_range(&mut self, start: u16, end: u16) -> u32 {
        debug_assert!(start <= end);
        let w = self.bits.words_mut();
        let before: u32 = w.iter().map(|x| x.count_ones()).sum();
        apply_range(w, start, end, |word, mask| *word |= mask);
        let after: u32 = w.iter().map(|x| x.count_ones()).sum();
        self.len = after;
        after - before
    }

    /// Clear `[start, end]` inclusive. Returns how many bits actually changed.
    pub fn remove_range(&mut self, start: u16, end: u16) -> u32 {
        debug_assert!(start <= end);
        let w = self.bits.words_mut();
        let before: u32 = w.iter().map(|x| x.count_ones()).sum();
        apply_range(w, start, end, |word, mask| *word &= !mask);
        let after: u32 = w.iter().map(|x| x.count_ones()).sum();
        self.len = after;
        before - after
    }

    /// The payload as words, decoding into an owned copy when the shared buffer
    /// is not 8-byte aligned.
    ///
    /// **The rule lives here so it cannot be got wrong in five places, and it
    /// was.** `try_words` answers `None` on an unaligned shared buffer, and
    /// every caller that turned that into an empty slice — or propagated it with
    /// `?` — produced a *wrong answer* rather than an error: `iter` yielded
    /// nothing, `min` and `max` and `select` said `None`, and `rank` said zero,
    /// for bitmaps that were not empty. `iter` was fixed on 2026-08-28 and the
    /// other four were left, which is how a fix that changes one call site
    /// leaves the class alive.
    ///
    /// Unreachable through the page store, where every slot is 64-byte
    /// aligned — which is exactly why nothing else catches it. It is reachable
    /// through `from_custom_allocation` over an imported `.roaring` mapping,
    /// which is what `decode_words` exists for.
    ///
    /// The unaligned arm costs one 8 KiB copy per call, the price `buffer.rs`
    /// already names for unaligned word access. A wrong answer is not cheaper.
    #[inline]
    pub(crate) fn words(&self) -> Cow<'_, [u64]> {
        match self.bits.try_words() {
            Some(w) => Cow::Borrowed(w),
            None => Cow::Owned(self.bits.decode_words()),
        }
    }

    #[inline]
    pub fn min(&self) -> Option<u16> {
        for (i, &x) in self.words().iter().enumerate() {
            if x != 0 {
                return Some((i as u32 * 64 + x.trailing_zeros()) as u16);
            }
        }
        None
    }

    #[inline]
    pub fn max(&self) -> Option<u16> {
        for (i, &x) in self.words().iter().enumerate().rev() {
            if x != 0 {
                return Some((i as u32 * 64 + (63 - x.leading_zeros())) as u16);
            }
        }
        None
    }

    pub fn rank(&self, v: u16) -> u32 {
        let w = self.words();
        let hi = word_of(v);
        let mut n: u32 = w[..hi].iter().map(|x| x.count_ones()).sum();
        // Bits strictly below `v` within its own word.
        let bit = (v & 63) as u32;
        if bit > 0 {
            n += (w[hi] & ((1u64 << bit) - 1)).count_ones();
        }
        n
    }

    pub fn select(&self, n: u32) -> Option<u16> {
        let w = self.words();
        let mut remaining = n;
        for (i, &x) in w.iter().enumerate() {
            let c = x.count_ones();
            if remaining < c {
                let mut word = x;
                for _ in 0..remaining {
                    word &= word - 1; // clear lowest set bit
                }
                return Some((i as u32 * 64 + word.trailing_zeros()) as u16);
            }
            remaining -= c;
        }
        None
    }

    /// Maximal runs of consecutive set bits, without allocating.
    pub fn run_count(&self) -> u32 {
        // Was `else { return 0 }` — the sixth site of the same defect, and
        // the one with teeth: `Container::optimize` uses this to decide whether
        // a run encoding is smaller, so a silent zero makes an unaligned bitmap
        // look like it has no runs at all.
        let owned;
        let w = match self.bits.try_words() {
            Some(w) => w,
            None => {
                owned = self.bits.decode_words();
                &owned[..]
            }
        };
        let mut runs = 0u32;
        let mut prev_high = false;
        for &x in w.iter() {
            if x == 0 {
                prev_high = false;
                continue;
            }
            // A run starts at each 0->1 transition within the word, plus one at
            // bit 0 if the previous word did not end set.
            runs += (x & !(x << 1)).count_ones();
            if x & 1 != 0 && prev_high {
                runs -= 1; // this word's first run continues the previous word's
            }
            prev_high = x & (1 << 63) != 0;
        }
        runs
    }

    /// Maximal runs as `(start, end)` inclusive pairs.
    pub fn runs(&self) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        // The seventh site. `runs()` feeding an empty vector to a run-container
        // build is a silently empty container, not an error.
        let owned;
        let w = match self.bits.try_words() {
            Some(w) => w,
            None => {
                owned = self.bits.decode_words();
                &owned[..]
            }
        };
        let mut start: Option<u32> = None;
        for (i, &x) in w.iter().enumerate() {
            if x == 0 {
                if let Some(s) = start.take() {
                    out.push((s as u16, (i as u32 * 64 - 1) as u16));
                }
                continue;
            }
            for b in 0..64u32 {
                let set = x & (1u64 << b) != 0;
                let pos = i as u32 * 64 + b;
                match (set, start) {
                    (true, None) => start = Some(pos),
                    (false, Some(s)) => {
                        out.push((s as u16, (pos - 1) as u16));
                        start = None;
                    }
                    _ => {}
                }
            }
        }
        if let Some(s) = start {
            out.push((s as u16, (CHUNK_CARD - 1) as u16));
        }
        out
    }

    /// Every set value, ascending.
    ///
    /// **This used to be `try_words().unwrap_or(&[])`, which made a bitmap
    /// on an unaligned shared buffer iterate as *empty*** — a wrong answer with
    /// no diagnostic, propagating through `ops::generic` ( the oracle kernel ),
    /// `OrdSet::iter`, and every consumer downstream.
    ///
    /// It was unreachable in production and remains so: slab bases are 2 MiB
    /// aligned, so every slot in every size class is 64-byte aligned and a
    /// container decoded from the page store always has words. But "unreachable
    /// today" and "silently wrong if it ever is reachable" are different
    /// properties, and only the second one is a landmine. The fallback
    /// `BitStore::decode_words` already existed and is what `buffer.rs`
    /// documents; the borrow is why it was not used, which the `Cow` fixes.
    pub fn iter(&self) -> BitmapIter<'_> {
        BitmapIter {
            words: self.words(),
            idx: 0,
            cur: 0,
            primed: false,
        }
    }
}

/// Apply `op` to every bit in `[start, end]` inclusive, with edge masking.
fn apply_range(w: &mut [u64], start: u16, end: u16, op: impl Fn(&mut u64, u64)) {
    let (sw, ew) = (word_of(start), word_of(end));
    let sb = (start & 63) as u32;
    let eb = (end & 63) as u32;
    if sw == ew {
        // Bits sb..=eb of one word.
        let mask = if eb == 63 {
            !0u64 << sb
        } else {
            ((1u64 << (eb + 1)) - 1) & (!0u64 << sb)
        };
        op(&mut w[sw], mask);
        return;
    }
    op(&mut w[sw], !0u64 << sb);
    for word in &mut w[sw + 1..ew] {
        op(word, !0u64);
    }
    let tail = if eb == 63 {
        !0u64
    } else {
        (1u64 << (eb + 1)) - 1
    };
    op(&mut w[ew], tail);
}

/// Iterator over a bitmap's set values.
///
/// Holds `Cow` rather than a borrow so an unaligned shared buffer can be
/// decoded into owned words instead of silently yielding nothing. Both fields
/// are private, so this is not a change to the type's public shape.
pub struct BitmapIter<'a> {
    words: Cow<'a, [u64]>,
    idx: usize,
    cur: u64,
    primed: bool,
}

impl Iterator for BitmapIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if !self.primed {
            self.cur = *self.words.first()?;
            self.primed = true;
        }
        loop {
            if self.cur != 0 {
                let b = self.cur.trailing_zeros();
                self.cur &= self.cur - 1;
                return Some((self.idx as u32 * 64 + b) as u16);
            }
            self.idx += 1;
            self.cur = *self.words.get(self.idx)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_remove_maintain_exact_len() {
        let mut b = BitmapContainer::zeroed();
        assert!(b.insert(5));
        assert!(!b.insert(5), "re-insert must report no change");
        assert!(b.insert(70));
        assert_eq!(b.len(), 2);
        assert!(b.remove(5));
        assert!(!b.remove(5));
        assert_eq!(b.len(), 1);
        assert_eq!(
            b.len(),
            b.bits.count_ones(),
            "cached len must match popcount"
        );
    }

    #[test]
    fn toggle_flips_and_tracks_len() {
        let mut b = BitmapContainer::zeroed();
        assert!(b.toggle(9));
        assert_eq!(b.len(), 1);
        assert!(!b.toggle(9));
        assert_eq!(b.len(), 0);
    }

    #[test]
    fn range_ops_handle_word_edges() {
        // Spanning a word boundary is where masking bugs live.
        let mut b = BitmapContainer::zeroed();
        assert_eq!(b.insert_range(60, 70), 11);
        assert_eq!(b.len(), 11);
        for v in 60..=70u16 {
            assert!(b.contains(v), "{v} should be set");
        }
        assert!(!b.contains(59));
        assert!(!b.contains(71));
        assert_eq!(b.remove_range(65, 65), 1);
        assert_eq!(b.len(), 10);
        assert!(!b.contains(65));
    }

    #[test]
    fn range_within_single_word_and_to_the_end() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(0, 63);
        assert_eq!(b.len(), 64);
        let mut c = BitmapContainer::zeroed();
        c.insert_range(0, 65535);
        assert_eq!(c.len(), 65536);
        assert!(c.is_full());
    }

    #[test]
    fn min_max_rank_select() {
        let mut b = BitmapContainer::zeroed();
        for v in [3u16, 100, 65535] {
            b.insert(v);
        }
        assert_eq!(b.min(), Some(3));
        assert_eq!(b.max(), Some(65535));
        assert_eq!(b.rank(3), 0);
        assert_eq!(b.rank(4), 1);
        assert_eq!(b.rank(65535), 2);
        assert_eq!(b.select(0), Some(3));
        assert_eq!(b.select(1), Some(100));
        assert_eq!(b.select(2), Some(65535));
        assert_eq!(b.select(3), None);
    }

    /// The regression `iter` was silently wrong on until 2026-08-28.
    ///
    /// An unaligned shared buffer makes `try_words` return `None`, and the old
    /// `unwrap_or(&[])` turned that into an empty iterator — a wrong answer,
    /// not an error. Unreachable through the page store, where every slot is
    /// 64-byte aligned, which is exactly why nothing else catches it.
    /// Every accessor, not just `iter`, on a bitmap whose buffer is unaligned.
    ///
    /// `iter` was fixed on 2026-08-28 and `min` / `max` / `rank` / `select`
    /// were left carrying the identical defect for another turn — they took the
    /// same `try_words` and turned `None` into a **wrong answer** rather than an
    /// error. Measured before the fix, on a bitmap holding every eighth ordinal:
    ///
    /// ```text
    /// min=None      ( want Some(0) )       max=None      ( want Some(65528) )
    /// rank(1000)=0  ( want 125 )           select(3)=None ( want Some(24) )
    /// ```
    ///
    /// So this test covers the *class*, not `iter`. A fix that repaired one
    /// call site is what left the others alive, and a test named after one call
    /// site is what let that pass.
    #[test]
    fn every_unaligned_bitmap_accessor_agrees_with_the_aligned_one() {
        use crate::BITMAP_BYTES;

        let want: Vec<u16> = (0u32..65_536).step_by(8).map(|v| v as u16).collect();
        let mut bytes = vec![0u8; BITMAP_BYTES + 8];
        for &v in &want {
            bytes[1 + (v as usize >> 3)] |= 1 << (v & 7);
        }
        let buf = arrow_buffer::Buffer::from_vec(bytes);
        let bits = BitStore::shared_from_bytes(&buf, 1).expect("room for a bitmap");
        assert!(
            bits.try_words().is_none(),
            "offset 1 must be unaligned, or this test proves nothing"
        );
        let unaligned = BitmapContainer {
            bits,
            len: want.len() as u32,
        };

        let mut aligned = BitmapContainer::zeroed();
        for &v in &want {
            aligned.insert(v);
        }

        assert_eq!(unaligned.min(), aligned.min(), "min");
        assert_eq!(unaligned.min(), Some(0));
        assert_eq!(unaligned.max(), aligned.max(), "max");
        assert_eq!(unaligned.max(), Some(65_528));
        for probe in [0u16, 1, 999, 1_000, 32_768, 65_535] {
            assert_eq!(unaligned.rank(probe), aligned.rank(probe), "rank({probe})");
        }
        assert_eq!(unaligned.rank(1_000), 125);
        for n in [0u32, 1, 3, 100, want.len() as u32 - 1, want.len() as u32] {
            assert_eq!(unaligned.select(n), aligned.select(n), "select({n})");
        }
        assert_eq!(unaligned.select(3), Some(24));
        assert_eq!(unaligned.contains(24), aligned.contains(24));
        assert_eq!(unaligned.contains(25), aligned.contains(25));
    }

    /// Sites six and seven of the same defect: `run_count` returned 0 and
    /// `runs` returned empty on an unaligned buffer.
    ///
    /// `run_count` is the one with teeth — `Container::optimize` uses it to
    /// decide whether a run encoding is smaller, so a silent zero makes a
    /// perfectly run-shaped bitmap look like it has none.
    #[test]
    fn an_unaligned_shared_bitmap_reports_its_runs() {
        use crate::BITMAP_BYTES;

        // Four clean runs, so both answers are easy to state.
        let spans = [(0u16, 99u16), (200, 299), (1000, 1000), (65_000, 65_535)];
        let mut bytes = vec![0u8; BITMAP_BYTES + 8];
        let mut n = 0u32;
        for &(lo, hi) in &spans {
            for v in lo..=hi {
                bytes[1 + (v as usize >> 3)] |= 1 << (v & 7);
                n += 1;
            }
        }
        let buf = arrow_buffer::Buffer::from_vec(bytes);
        let bits = BitStore::shared_from_bytes(&buf, 1).expect("room for a bitmap");
        assert!(bits.try_words().is_none(), "the fixture must be unaligned");

        let c = BitmapContainer { bits, len: n };
        assert_eq!(c.run_count(), spans.len() as u32);
        assert_eq!(c.runs(), spans.to_vec());

        // And the aligned form agrees, so the two paths cannot drift.
        let mut aligned = BitmapContainer::zeroed();
        for &(lo, hi) in &spans {
            for v in lo..=hi {
                aligned.insert(v);
            }
        }
        assert_eq!(c.run_count(), aligned.run_count());
        assert_eq!(c.runs(), aligned.runs());
    }

    #[test]
    fn an_unaligned_shared_bitmap_still_iterates_its_values() {
        use crate::BITMAP_BYTES;

        // Lay the payload one byte past an aligned allocation.
        let want: Vec<u16> = (0u32..65_536).step_by(8).map(|v| v as u16).collect();
        let mut bytes = vec![0u8; BITMAP_BYTES + 8];
        for &v in &want {
            bytes[1 + (v as usize >> 3)] |= 1 << (v & 7);
        }
        let buf = arrow_buffer::Buffer::from_vec(bytes);
        let bits = BitStore::shared_from_bytes(&buf, 1).expect("room for a bitmap");
        assert!(
            bits.try_words().is_none(),
            "offset 1 must be unaligned, or this test proves nothing"
        );

        let c = BitmapContainer {
            bits,
            len: want.len() as u32,
        };
        assert_eq!(c.iter().collect::<Vec<u16>>(), want);
        // And the aligned form must still agree, so the two paths cannot drift.
        let mut aligned = BitmapContainer::zeroed();
        for &v in &want {
            aligned.insert(v);
        }
        assert_eq!(
            c.iter().collect::<Vec<u16>>(),
            aligned.iter().collect::<Vec<u16>>()
        );
    }

    #[test]
    fn iter_yields_ascending_values() {
        let mut b = BitmapContainer::zeroed();
        let vals = [0u16, 1, 63, 64, 65, 1000, 65535];
        for &v in &vals {
            b.insert(v);
        }
        assert_eq!(b.iter().collect::<Vec<_>>(), vals);
    }

    #[test]
    fn run_count_matches_runs() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(0, 10);
        b.insert_range(20, 30);
        b.insert(100);
        assert_eq!(b.run_count(), 3);
        assert_eq!(b.runs(), vec![(0, 10), (20, 30), (100, 100)]);
    }

    #[test]
    fn run_count_across_word_boundary_is_one_run() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(60, 70);
        assert_eq!(
            b.run_count(),
            1,
            "a run spanning words must not be double counted"
        );
        assert_eq!(b.runs(), vec![(60, 70)]);
    }

    #[test]
    fn full_bitmap_is_one_run() {
        let mut b = BitmapContainer::zeroed();
        b.insert_range(0, 65535);
        assert_eq!(b.run_count(), 1);
        assert_eq!(b.runs(), vec![(0, 65535)]);
    }
}
