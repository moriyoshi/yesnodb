//! Folding across constituents, and the inverse image back out.
//!
//! [`OrdSet::view_fold`] reduces a view's `n` constituents to one set over the
//! same logical ordinals; [`OrdSet::view_expand`] does the opposite, filling
//! every constituent's slot for each logical ordinal present.
//!
//! # A fold is not a restriction, and the difference is the whole algebra
//!
//! `docs/formal-model.md` §14 Proposition 18 says a prefix-window restriction is
//! a homomorphism of the **whole** Boolean signature and pushes down with no
//! side condition — "a planner that declines to push a restriction down is
//! declining on cost; it can never be declining on correctness". A fold is
//! nothing like that. Each monoid is exact for exactly one operator:
//!
//! ```text
//!   Any    ( ∃ )   exact for ∪      only  ⊆  for ∩
//!   All    ( ∀ )   exact for ∩      only  ⊇  for ∪
//!   Parity ( ⊕ )   exact for △      neither for ∩ or ∪
//! ```
//!
//! and **nothing** commutes with `\`. So a fold may never be pushed through a
//! binary operator unconditionally, and a reader who has internalised
//! Proposition 18 will assume it may and be wrong.
//!
//! **The inverse direction is free.** [`OrdSet::view_expand`] is a
//! homomorphism of the entire signature, because inverse images always are, and
//! `∃ ⊣ ⁻¹ ⊣ ∀` is an adjoint triple. That is why `expand( coarse ) ∩ fine`
//! composes with everything while `fold( a ∩ b )` does not.
//!
//! # This generalises the planner's occupancy statistic
//!
//! §7.1's `α( X ) = { ⌊( p − β ) / 2^τ⌋ }` is [`Reduce::Any`] at stride 1 on the
//! prefix axis, and its Proposition 9 ( sound omission ) is the `∩` one-sided law
//! above. A fold is therefore a **caller-declared zone map**, sound in exactly
//! the same direction: it can prove disjointness, never non-emptiness.
//!
//! # Which arm answers
//!
//! Selecting each constituent and combining with ordinary set algebra is correct
//! for every descriptor, reuses the tuned kernels, and is the **oracle** — it
//! is never deleted when an arm is faster. Under
//! [`ViewLayout::Interleaved`](super::ViewLayout::Interleaved) that costs
//! `O( n · nnz )`, because each `view_select` is itself a strided filter over
//! everything; a single grouped walk does it in `O( nnz )`, and that is the
//! specialised arm. For aligned bitmap-only inputs at arity 2, 4 or 8, a
//! byte reducer folds physical bitmap chunks into logical output chunks
//! without enumerating set bits. Little-endian AArch64 uses a NEON reducer
//! for these same chunks after runtime feature detection; other hosts use the
//! scalar byte table. Mixed containers and unaligned shared bitmap payloads
//! retain the grouped walk.

use super::{View, ViewLayout};
use crate::container::{BitmapContainer, Container};
use crate::{
    chunk_base, chunk_window, split, ChunkStream, CodecError, OrdSet, Prefix48, Result,
    BITMAP_WORDS, ORDINAL_MAX,
};

/// How a fold combines the constituents at one logical ordinal.
///
/// Closed at three for the reason [`Semiring`](crate::matrix::Semiring) is
/// closed at two: these are the monoids available on packed bits. `\` is not
/// associative and `∩`'s identity is the all-ones vector, so neither can be a
/// fold. One kernel serves all three — the walk counts, and each variant reads
/// its answer off the count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Reduce {
    /// Set when **any** constituent holds the logical ordinal — their union.
    Any,
    /// Set when **every** constituent holds it — their intersection.
    All,
    /// Set when an **odd** number hold it — their symmetric difference.
    Parity,
}

impl Reduce {
    /// Does a logical ordinal held by `count` of `sets` constituents survive?
    #[inline]
    fn keep(self, count: u64, sets: u64) -> bool {
        match self {
            Reduce::Any => count > 0,
            Reduce::All => count == sets,
            Reduce::Parity => count % 2 == 1,
        }
    }
}

/// The grouped interleaved walk shared by resident and streamed inputs.
struct InterleavedFold {
    sets: u64,
    reduce: Reduce,
    out: Vec<u64>,
    current: Option<u64>,
    count: u64,
}

impl InterleavedFold {
    fn new(view: &View, reduce: Reduce) -> Result<Self> {
        view.check()?;
        if !matches!(view.layout(), ViewLayout::Interleaved) {
            return Err(CodecError::Invariant(
                "streaming view fold requires an interleaved view",
            ));
        }
        Ok(Self {
            sets: u64::from(view.sets()),
            reduce,
            out: Vec::new(),
            current: None,
            count: 0,
        })
    }

    fn push(&mut self, prefix: Prefix48, container: &Container) {
        let base = chunk_base(prefix);
        for low in container.iter() {
            let logical = (base | u64::from(low)) / self.sets;
            if self.current != Some(logical) {
                self.flush();
                self.current = Some(logical);
                self.count = 0;
            }
            self.count += 1;
        }
    }

    fn flush(&mut self) {
        if let Some(logical) = self.current {
            if self.reduce.keep(self.count, self.sets) {
                self.out.push(logical);
            }
        }
    }

    fn finish(mut self) -> OrdSet {
        self.flush();
        let mut out = OrdSet::from_sorted_slice(&self.out);
        out.optimize();
        out
    }
}

/// Fold an interleaved view while consuming a prefix-ordered chunk stream.
///
/// Physical order is logical-group order only for
/// [`ViewLayout::Interleaved`], so blocked views are rejected rather than
/// silently grouped incorrectly. The output set is the only materialized set;
/// the packed input remains a stream. Malformed stream ordering and the
/// reserved `u64::MAX` ordinal are reported as errors.
pub fn stream_interleaved_view_fold(
    stream: &mut dyn ChunkStream,
    view: &View,
    reduce: Reduce,
) -> Result<OrdSet> {
    let mut fold = InterleavedFold::new(view, reduce)?;
    let mut last_prefix = None;
    while let Some((prefix, container)) = stream.next_chunk()? {
        super::validate_stream_chunk(&mut last_prefix, prefix, &container)?;
        fold.push(prefix, &container);
    }
    Ok(fold.finish())
}

/// The low bit of each sets-wide group becomes one output bit. Compile-time
/// tables keep the per-byte hot loop independent of the reduction branches.
const fn fold_byte(byte: u8, sets: usize, reduce: Reduce) -> u8 {
    let mut out = 0u8;
    let mut group = 0;
    while group < 8 / sets {
        let bits = ((byte as u16 >> (group * sets)) & ((1u16 << sets) - 1)) as u8;
        let keep = match reduce {
            Reduce::Any => bits != 0,
            Reduce::All => bits.count_ones() as usize == sets,
            Reduce::Parity => bits.count_ones() % 2 == 1,
        };
        if keep {
            out |= 1 << group;
        }
        group += 1;
    }
    out
}

const fn fold_byte_table(sets: usize, reduce: Reduce) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut byte = 0;
    while byte < 256 {
        table[byte] = fold_byte(byte as u8, sets, reduce);
        byte += 1;
    }
    table
}

const FOLD_ANY_2: [u8; 256] = fold_byte_table(2, Reduce::Any);
const FOLD_ALL_2: [u8; 256] = fold_byte_table(2, Reduce::All);
const FOLD_PARITY_2: [u8; 256] = fold_byte_table(2, Reduce::Parity);
const FOLD_ANY_4: [u8; 256] = fold_byte_table(4, Reduce::Any);
const FOLD_ALL_4: [u8; 256] = fold_byte_table(4, Reduce::All);
const FOLD_PARITY_4: [u8; 256] = fold_byte_table(4, Reduce::Parity);
const FOLD_ANY_8: [u8; 256] = fold_byte_table(8, Reduce::Any);
const FOLD_ALL_8: [u8; 256] = fold_byte_table(8, Reduce::All);
const FOLD_PARITY_8: [u8; 256] = fold_byte_table(8, Reduce::Parity);

fn fold_table(sets: u32, reduce: Reduce) -> Option<&'static [u8; 256]> {
    match (sets, reduce) {
        (2, Reduce::Any) => Some(&FOLD_ANY_2),
        (2, Reduce::All) => Some(&FOLD_ALL_2),
        (2, Reduce::Parity) => Some(&FOLD_PARITY_2),
        (4, Reduce::Any) => Some(&FOLD_ANY_4),
        (4, Reduce::All) => Some(&FOLD_ALL_4),
        (4, Reduce::Parity) => Some(&FOLD_PARITY_4),
        (8, Reduce::Any) => Some(&FOLD_ANY_8),
        (8, Reduce::All) => Some(&FOLD_ALL_8),
        (8, Reduce::Parity) => Some(&FOLD_PARITY_8),
        _ => None,
    }
}

/// Try an architecture-specific vector reducer for one complete bitmap chunk.
///
/// Dispatch is per 8 KiB physical container, not per word or logical row.
/// Unsupported architectures and CPUs keep the scalar byte table.
#[inline]
fn fold_words_simd(words: &[u64], out: &mut [u64], sets: usize, reduce: Reduce) -> bool {
    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: NEON was detected; the caller supplies one complete bitmap
        // and the corresponding 1024 / sets-word output segment (bound B9).
        match sets {
            2 => unsafe { neon::fold2(words, out, reduce) },
            4 => unsafe { neon::fold4(words, out, reduce) },
            8 => unsafe { neon::fold8(words, out, reduce) },
            _ => return false,
        }
        return true;
    }
    #[cfg(all(target_arch = "x86_64", target_endian = "little"))]
    if std::arch::is_x86_feature_detected!("ssse3") {
        // SAFETY: SSSE3 was detected; the caller supplies one complete bitmap
        // and the corresponding 1024 / sets-word output segment (bound B9).
        match sets {
            2 => unsafe { sse::fold2(words, out, reduce) },
            4 => unsafe { sse::fold4(words, out, reduce) },
            8 => unsafe { sse::fold8(words, out, reduce) },
            _ => return false,
        }
        return true;
    }
    let _ = (words, out, sets, reduce);
    false
}

/// The x86_64 counterpart of [`neon`], under the same bound B9.
///
/// Two NEON instructions this kernel is built on have no x86 equivalent, and
/// the substitutes are what the arms differ by.
///
/// `vshlq_u8` shifts each byte by its own amount, which is how the NEON arms
/// move a nibble's folded bits into their output positions. x86 has no
/// per-byte variable shift at any width. Two replacements are used instead:
/// the per-nibble shift is folded into a **second lookup table** whose entries
/// are pre-shifted, costing nothing at all, and the cross-byte positioning
/// becomes `pmaddubsw` / `pmaddwd`, which multiply by a per-lane weight and
/// add adjacent lanes in one instruction -- a shift and NEON's following
/// pairwise add, fused.
///
/// For arity 8 the port is abandoned outright in favour of `pmovmskb`, which
/// takes the high bit of all 16 bytes into a 16-bit integer. That is precisely
/// one output word's worth of bits per instruction, and NEON has no equivalent
/// -- its arm needs a compare, a positioning shift and a three-level pairwise
/// tree to do the same gather. Arity 8 is therefore the cheapest x86 fold and
/// the most expensive NEON one, which is the whole lesson of
/// `LTM/simd-arch-arms-and-kernel-selection.md` reappearing: a technique's
/// value is a property of the instruction set, not of the algorithm.
///
/// 128-bit rather than AVX2 deliberately. Every arity here ends by packing
/// bytes drawn from across the whole vector, and AVX2's byte shuffles and
/// `packus` work inside 128-bit halves, so each kernel would need a
/// cross-lane permute per iteration to repair the order. That is the trade
/// `ops::array` measured and rejected. The count kernel takes AVX2 precisely
/// because it has no such step.
#[cfg(all(target_arch = "x86_64", target_endian = "little"))]
mod sse {
    use super::{Reduce, BITMAP_WORDS};
    use std::arch::x86_64::*;

    /// Folded bits for a nibble, in the output positions the *low* nibble of
    /// a byte owns.
    const fn nibble_lo(sets: usize, reduce: Reduce) -> [u8; 16] {
        let mut table = [0u8; 16];
        let mut nibble = 0;
        while nibble < 16 {
            table[nibble] = super::fold_byte(nibble as u8, sets, reduce);
            nibble += 1;
        }
        table
    }

    /// The same, pre-shifted into the positions the *high* nibble owns. A
    /// nibble carries `4 / sets` groups, so that is the shift.
    const fn nibble_hi(sets: usize, reduce: Reduce) -> [u8; 16] {
        let mut table = nibble_lo(sets, reduce);
        let mut nibble = 0;
        while nibble < 16 {
            table[nibble] <<= 4 / sets;
            nibble += 1;
        }
        table
    }

    /// `0x80` when the nibble has odd population, so that a byte's parity
    /// lands in the bit `pmovmskb` reads.
    const fn parity_high_bit() -> [u8; 16] {
        let mut table = [0u8; 16];
        let mut nibble = 0;
        while nibble < 16 {
            table[nibble] = if (nibble as u8).count_ones() % 2 == 1 {
                0x80
            } else {
                0
            };
            nibble += 1;
        }
        table
    }

    const ANY2_LO: [u8; 16] = nibble_lo(2, Reduce::Any);
    const ANY2_HI: [u8; 16] = nibble_hi(2, Reduce::Any);
    const ALL2_LO: [u8; 16] = nibble_lo(2, Reduce::All);
    const ALL2_HI: [u8; 16] = nibble_hi(2, Reduce::All);
    const PARITY2_LO: [u8; 16] = nibble_lo(2, Reduce::Parity);
    const PARITY2_HI: [u8; 16] = nibble_hi(2, Reduce::Parity);
    const ANY4_LO: [u8; 16] = nibble_lo(4, Reduce::Any);
    const ANY4_HI: [u8; 16] = nibble_hi(4, Reduce::Any);
    const ALL4_LO: [u8; 16] = nibble_lo(4, Reduce::All);
    const ALL4_HI: [u8; 16] = nibble_hi(4, Reduce::All);
    const PARITY4_LO: [u8; 16] = nibble_lo(4, Reduce::Parity);
    const PARITY4_HI: [u8; 16] = nibble_hi(4, Reduce::Parity);
    const PARITY_BIT7: [u8; 16] = parity_high_bit();

    #[inline]
    fn tables2(reduce: Reduce) -> (&'static [u8; 16], &'static [u8; 16]) {
        match reduce {
            Reduce::Any => (&ANY2_LO, &ANY2_HI),
            Reduce::All => (&ALL2_LO, &ALL2_HI),
            Reduce::Parity => (&PARITY2_LO, &PARITY2_HI),
        }
    }

    #[inline]
    fn tables4(reduce: Reduce) -> (&'static [u8; 16], &'static [u8; 16]) {
        match reduce {
            Reduce::Any => (&ANY4_LO, &ANY4_HI),
            Reduce::All => (&ALL4_LO, &ALL4_HI),
            Reduce::Parity => (&PARITY4_LO, &PARITY4_HI),
        }
    }

    /// B9 for n=2: 16 source bytes produce 8 destination bytes, 512 complete
    /// iterations, no tail.
    ///
    /// # Safety
    ///
    /// Requires SSSE3, exactly 1024 source words, and 512 output words.
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn fold2(words: &[u64], out: &mut [u64], reduce: Reduce) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 2, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        let (lo_table, hi_table) = tables2(reduce);
        // SAFETY: B9 bounds all 16-byte loads and 8-byte stores; each store
        // begins at i / 2, a multiple of eight, inside the output half-chunk.
        // Both `pshufb` indices are masked to 0..15. `pmaddubsw` sums
        // 15 + 15 * 16 = 255 at most, so it cannot saturate, and `packus`
        // therefore never clamps.
        unsafe {
            let lo_lut = _mm_loadu_si128(lo_table.as_ptr().cast());
            let hi_lut = _mm_loadu_si128(hi_table.as_ptr().cast());
            let low_mask = _mm_set1_epi8(0x0f);
            let weights = _mm_set1_epi16(0x1001);
            let mut i = 0usize;
            while i < src.len() {
                let input = _mm_loadu_si128(src.as_ptr().add(i).cast());
                let lo = _mm_shuffle_epi8(lo_lut, _mm_and_si128(input, low_mask));
                let hi =
                    _mm_shuffle_epi8(hi_lut, _mm_and_si128(_mm_srli_epi16(input, 4), low_mask));
                let folded = _mm_or_si128(lo, hi);
                let pairs = _mm_maddubs_epi16(folded, weights);
                let packed = _mm_packus_epi16(pairs, pairs);
                _mm_storel_epi64(dst.as_mut_ptr().add(i / 2).cast(), packed);
                i += 16;
            }
        }
    }

    /// B9 for n=4: 16 source bytes produce 4 destination bytes.
    ///
    /// # Safety
    ///
    /// Requires SSSE3, exactly 1024 source words, and 256 output words.
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn fold4(words: &[u64], out: &mut [u64], reduce: Reduce) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 4, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        let (lo_table, hi_table) = tables4(reduce);
        // SAFETY: B9 bounds every 16-byte load and 4-byte store; i / 4 is a
        // multiple of four inside the output quarter-chunk. Neither multiply
        // step can overflow its lane: `pmaddubsw` reaches 3 + 3 * 4 = 15 and
        // `pmaddwd` reaches 15 + 15 * 16 = 255.
        unsafe {
            let lo_lut = _mm_loadu_si128(lo_table.as_ptr().cast());
            let hi_lut = _mm_loadu_si128(hi_table.as_ptr().cast());
            let low_mask = _mm_set1_epi8(0x0f);
            let byte_weights = _mm_set1_epi16(0x0401);
            let word_weights = _mm_set1_epi32(0x0010_0001);
            let gather = _mm_setr_epi8(0, 4, 8, 12, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1);
            let mut i = 0usize;
            while i < src.len() {
                let input = _mm_loadu_si128(src.as_ptr().add(i).cast());
                let lo = _mm_shuffle_epi8(lo_lut, _mm_and_si128(input, low_mask));
                let hi =
                    _mm_shuffle_epi8(hi_lut, _mm_and_si128(_mm_srli_epi16(input, 4), low_mask));
                let folded = _mm_or_si128(lo, hi);
                let pairs = _mm_maddubs_epi16(folded, byte_weights);
                let quads = _mm_madd_epi16(pairs, word_weights);
                let packed = _mm_shuffle_epi8(quads, gather);
                let four = _mm_cvtsi128_si32(packed) as u32;
                dst.as_mut_ptr()
                    .add(i / 4)
                    .cast::<u32>()
                    .write_unaligned(four);
                i += 16;
            }
        }
    }

    /// B9 for n=8: 16 source bytes produce 2 destination bytes -- one output
    /// bit per input byte, which is exactly what `pmovmskb` returns.
    ///
    /// # Safety
    ///
    /// Requires SSSE3, exactly 1024 source words, and 128 output words.
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn fold8(words: &[u64], out: &mut [u64], reduce: Reduce) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 8, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        // SAFETY: B9 bounds all 16-byte loads and the two byte stores at
        // i / 8, which is even and below the output eighth-chunk's length.
        // `pmovmskb` reads bit 7 of each byte; every branch below places the
        // fold's answer there and nothing else in the byte is read.
        unsafe {
            let zero = _mm_setzero_si128();
            let full = _mm_set1_epi8(-1);
            let parity_lut = _mm_loadu_si128(PARITY_BIT7.as_ptr().cast());
            let low_mask = _mm_set1_epi8(0x0f);
            let mut i = 0usize;
            while i < src.len() {
                let input = _mm_loadu_si128(src.as_ptr().add(i).cast());
                let bits = match reduce {
                    // `cmpeq` against zero marks the bytes that are *empty*,
                    // so the complement of the mask is the union.
                    Reduce::Any => !(_mm_movemask_epi8(_mm_cmpeq_epi8(input, zero)) as u16),
                    Reduce::All => _mm_movemask_epi8(_mm_cmpeq_epi8(input, full)) as u16,
                    Reduce::Parity => {
                        let lo = _mm_shuffle_epi8(parity_lut, _mm_and_si128(input, low_mask));
                        let hi = _mm_shuffle_epi8(
                            parity_lut,
                            _mm_and_si128(_mm_srli_epi16(input, 4), low_mask),
                        );
                        _mm_movemask_epi8(_mm_xor_si128(lo, hi)) as u16
                    }
                };
                let [low, high] = bits.to_le_bytes();
                dst[i / 8] = low;
                dst[i / 8 + 1] = high;
                i += 16;
            }
        }
    }
}

#[cfg(all(target_arch = "aarch64", target_endian = "little"))]
mod neon {
    use super::{Reduce, BITMAP_WORDS};
    use std::arch::aarch64::*;

    const fn nibble_table(sets: usize, reduce: Reduce) -> [u8; 16] {
        let mut table = [0u8; 16];
        let mut nibble = 0;
        while nibble < 16 {
            table[nibble] = super::fold_byte(nibble as u8, sets, reduce);
            nibble += 1;
        }
        table
    }

    const ANY2: [u8; 16] = nibble_table(2, Reduce::Any);
    const ALL2: [u8; 16] = nibble_table(2, Reduce::All);
    const PARITY2: [u8; 16] = nibble_table(2, Reduce::Parity);
    const ANY4: [u8; 16] = nibble_table(4, Reduce::Any);
    const ALL4: [u8; 16] = nibble_table(4, Reduce::All);
    const PARITY4: [u8; 16] = nibble_table(4, Reduce::Parity);
    const SHIFTS2: [i8; 16] = [0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4, 0, 4];
    const SHIFTS4: [i8; 16] = [0, 2, 4, 6, 0, 2, 4, 6, 0, 2, 4, 6, 0, 2, 4, 6];
    const SHIFTS8: [i8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 0, 1, 2, 3, 4, 5, 6, 7];

    /// B9: exactly 1024 input words (8192 bytes) become 256 output words
    /// (2048 bytes). Each 16-byte load produces one 4-byte store. The loop
    /// advances by 16 and performs exactly 512 iterations, with no tail.
    ///
    /// # Safety
    ///
    /// Requires NEON and B9. The caller's output quarter must be zeroed or
    /// exclusively owned because this kernel overwrites it.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn fold4(words: &[u64], out: &mut [u64], reduce: Reduce) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 4, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        let nibble = match reduce {
            Reduce::Any => &ANY4,
            Reduce::All => &ALL4,
            Reduce::Parity => &PARITY4,
        };
        // SAFETY: B9 bounds every 16-byte load and 4-byte store. The table
        // and shifts are 16-byte arrays, and output stores are 4-byte aligned
        // because the destination is a u64 slice and i / 4 is a multiple of 4.
        unsafe {
            let table = vld1q_u8(nibble.as_ptr());
            let shifts = vld1q_s8(SHIFTS4.as_ptr());
            let mask = vdupq_n_u8(15);
            let mut i = 0usize;
            while i < src.len() {
                let input = vld1q_u8(src.as_ptr().add(i));
                let lo = vqtbl1q_u8(table, vandq_u8(input, mask));
                let hi = vshlq_n_u8(vqtbl1q_u8(table, vshrq_n_u8(input, 4)), 1);
                let positioned = vshlq_u8(vorrq_u8(lo, hi), shifts);
                let pairs = vpaddlq_u8(positioned);
                let groups = vpaddlq_u16(pairs);
                let packed16 = vmovn_u32(groups);
                let packed8 = vmovn_u16(vcombine_u16(packed16, vdup_n_u16(0)));
                vst1_lane_u32(
                    dst.as_mut_ptr().add(i / 4).cast::<u32>(),
                    vreinterpret_u32_u8(packed8),
                    0,
                );
                i += 16;
            }
        }
    }

    /// B9 for n=2: 16 source bytes produce 8 destination bytes.
    ///
    /// # Safety
    ///
    /// Requires NEON, exactly 1024 source words, and 512 output words.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn fold2(words: &[u64], out: &mut [u64], reduce: Reduce) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 2, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        let nibble = match reduce {
            Reduce::Any => &ANY2,
            Reduce::All => &ALL2,
            Reduce::Parity => &PARITY2,
        };
        // SAFETY: B9 bounds all 16-byte loads and 8-byte stores. Each store
        // begins at i / 2, a multiple of eight, inside the output half-chunk.
        unsafe {
            let table = vld1q_u8(nibble.as_ptr());
            let shifts = vld1q_s8(SHIFTS2.as_ptr());
            let mask = vdupq_n_u8(15);
            let mut i = 0usize;
            while i < src.len() {
                let input = vld1q_u8(src.as_ptr().add(i));
                let lo = vqtbl1q_u8(table, vandq_u8(input, mask));
                let hi = vshlq_n_u8(vqtbl1q_u8(table, vshrq_n_u8(input, 4)), 2);
                let positioned = vshlq_u8(vorrq_u8(lo, hi), shifts);
                let packed = vmovn_u16(vpaddlq_u8(positioned));
                vst1_u8(dst.as_mut_ptr().add(i / 2), packed);
                i += 16;
            }
        }
    }

    /// B9 for n=8: 16 source bytes produce 2 destination bytes.
    ///
    /// # Safety
    ///
    /// Requires NEON, exactly 1024 source words, and 128 output words.
    pub(super) unsafe fn fold8(words: &[u64], out: &mut [u64], reduce: Reduce) {
        match reduce {
            Reduce::Any => unsafe { fold8_op::<0>(words, out) },
            Reduce::All => unsafe { fold8_op::<1>(words, out) },
            Reduce::Parity => unsafe { fold8_op::<2>(words, out) },
        }
    }

    #[target_feature(enable = "neon")]
    unsafe fn fold8_op<const OP: u8>(words: &[u64], out: &mut [u64]) {
        debug_assert_eq!(words.len(), BITMAP_WORDS, "B9");
        debug_assert_eq!(out.len(), BITMAP_WORDS / 8, "B9");
        let src = bytemuck::cast_slice::<u64, u8>(words);
        let dst = bytemuck::cast_slice_mut::<u64, u8>(out);
        // SAFETY: B9 bounds all 16-byte loads and two byte stores. Pairwise
        // additions combine disjoint positioned bits, so no carries cross
        // logical output positions.
        unsafe {
            let zero = vdupq_n_u8(0);
            let one = vdupq_n_u8(1);
            let full = vdupq_n_u8(u8::MAX);
            let shifts = vld1q_s8(SHIFTS8.as_ptr());
            let mut i = 0usize;
            while i < src.len() {
                let input = vld1q_u8(src.as_ptr().add(i));
                let bits = match OP {
                    0 => vandq_u8(vcgtq_u8(input, zero), one),
                    1 => vandq_u8(vceqq_u8(input, full), one),
                    2 => vandq_u8(vcntq_u8(input), one),
                    _ => unreachable!("only Any, All and Parity are instantiated"),
                };
                let positioned = vshlq_u8(bits, shifts);
                let packed = vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(positioned)));
                dst[i / 8] = vgetq_lane_u64(packed, 0) as u8;
                dst[i / 8 + 1] = vgetq_lane_u64(packed, 1) as u8;
                i += 16;
            }
        }
    }
}

/// How much one `insert_range` costs, in units of one bulk-sorted ordinal.
///
/// **Measured, not tuned.** From `benches/view.rs` at `sets = 4` over 200 000
/// logical ordinals: the interval path spent ~207 ns per interval where the
/// generic path spent ~5.7 ns per emitted slot, a ratio near 36. Break-even is
/// therefore `intervals · 36 == slots`, which is what
/// [`OrdSet::expansion_coalesces`] tests.
///
/// It is a property of two *algorithms* — a container insert against a sorted
/// bulk build — not of this module, so it should move only when one of those
/// changes. Re-derive it by re-running `view/expand` and comparing the two rows
/// at a density where they are close, rather than by adjusting it until a
/// benchmark looks better.
const EXPAND_INTERVAL_COST: u64 = 36;

/// How many leading constituents of `v` can hold any of `s`'s ordinals.
///
/// A companion to [`View::addressable_sets`], which bounds by what the ordinal
/// space can address; this bounds by what the *data* reaches, which is the
/// tighter question when a `Blocked` stride is small enough that every
/// constituent is addressable and almost all are empty.
///
/// Both layouts put a constituent's ordinals above `i · stride` ( blocked ) or
/// give it residue `i` mod `sets` ( interleaved ), so in each case no
/// constituent past this index can contain an ordinal at or below the maximum.
fn occupied_sets(s: &OrdSet, v: &View) -> u32 {
    let Some(max) = s.max() else {
        return 0;
    };
    let last = match v.layout() {
        ViewLayout::Interleaved => max,
        ViewLayout::Blocked { stride } => {
            if stride == 0 {
                return 0;
            }
            max / stride
        }
    };
    last.saturating_add(1).min(v.sets() as u64) as u32
}

/// Accumulates ascending, half-open ordinal intervals into containers.
///
/// **Intervals, never an ordinal list.** Expanding multiplies cardinality by
/// the constituent count, so a `Vec<u64>` would be `O( output ordinals )` to
/// describe something whose run form is `O( output runs )`. Consecutive logical
/// ordinals expand to *adjacent* intervals under `Interleaved`, so a contiguous
/// input collapses to one run.
#[derive(Default)]
struct IntervalBuilder {
    chunks: Vec<(Prefix48, Container)>,
}

impl IntervalBuilder {
    /// Add `[lo, hi)`. Calls must be ascending and non-overlapping.
    fn add(&mut self, lo: u64, hi: u64) {
        if hi <= lo {
            return;
        }
        let (p_lo, _) = split(lo);
        let (p_hi, _) = split(hi - 1);
        for p in p_lo..=p_hi {
            let Some((l, h)) = chunk_window(p, lo, hi) else {
                continue;
            };
            if self.chunks.last().map(|(pp, _)| *pp) != Some(p) {
                self.chunks.push((p, Container::from_sorted(&[])));
            }
            let c = &mut self.chunks.last_mut().expect("just pushed").1;
            // `chunk_window` is half-open and `insert_range` is inclusive; `h`
            // may be CHUNK_CARD, so `h - 1` is the largest `u16`.
            c.insert_range(l as u16, (h - 1) as u16);
        }
    }

    fn build(mut self) -> OrdSet {
        for (_, c) in self.chunks.iter_mut() {
            c.ensure_demoted();
        }
        let mut s = OrdSet::from_chunks(self.chunks);
        s.optimize();
        s
    }
}

impl OrdSet {
    /// Reduce every constituent to one set over the shared logical ordinals.
    ///
    /// The result is bounded by the constituents rather than by the logical
    /// universe — a union is at most their total cardinality, an intersection at
    /// most the smallest — so no fold has to enumerate a domain. That is why
    /// there is no "count of addressable logical ordinals" anywhere here, and why
    /// [`Reduce::All`] does not need the vacuous-truth case a padded packing
    /// would force.
    pub fn view_fold(&self, v: &View, reduce: Reduce) -> OrdSet {
        if v.check().is_err() {
            return OrdSet::new();
        }
        if let Some(out) = self.fold_interleaved_bitmaps(v, reduce) {
            return out;
        }
        if let Some(out) = self.fold_interleaved(v, reduce) {
            return out;
        }
        self.fold_via_select(v, reduce)
    }

    /// The oracle: extract each constituent and combine with set algebra.
    ///
    /// Correct for every descriptor, and it reuses the tuned pairwise kernels
    /// rather than reimplementing them. Never deleted because an arm is
    /// faster — same contract as [`ops::generic`](crate::ops::generic).
    fn fold_via_select(&self, v: &View, reduce: Reduce) -> OrdSet {
        // Constituents above this hold nothing, whatever the descriptor says.
        // Bounding by `sets` instead makes the cost proportional to a `u32` a
        // client chose rather than to the data — four billion `view_select`
        // calls against a set that might hold one ordinal.
        let occupied = occupied_sets(self, v);
        // `Any` and `Parity` take an empty constituent as their identity, so
        // stopping early drops nothing. `All` does not: an empty constituent
        // empties the intersection, so a truncated loop must answer empty.
        if occupied < v.sets() && matches!(reduce, Reduce::All) {
            return OrdSet::new();
        }
        let mut acc: Option<OrdSet> = None;
        for i in 0..occupied {
            let s = self.view_select(v, i);
            acc = Some(match acc {
                None => s,
                Some(a) => match reduce {
                    Reduce::Any => a.or(&s),
                    Reduce::All => a.and(&s),
                    Reduce::Parity => a.xor(&s),
                },
            });
        }
        let mut out = acc.unwrap_or_default();
        out.optimize();
        out
    }

    /// Each physical chunk contributes 65_536 / n consecutive logical bits.
    /// Since n divides both 65_536 and 64, n consecutive physical chunks fit
    /// exactly in one logical output chunk and n input words fill one output
    /// word. Empty source prefixes contribute zeroes without being visited.
    /// Decline the entire arm on a non-bitmap or unaligned shared payload so
    /// the grouped ordinal walk remains the one mixed-kind implementation.
    fn fold_interleaved_bitmaps(&self, v: &View, reduce: Reduce) -> Option<OrdSet> {
        if v.layout() != ViewLayout::Interleaved {
            return None;
        }
        let table = fold_table(v.sets(), reduce)?;
        if self.is_empty() {
            return Some(OrdSet::new());
        }
        if self
            .chunks()
            .any(|(_, container)| crate::unstable_arrow::bitmap_words(container).is_none())
        {
            return None;
        }

        let n = v.sets() as usize;
        let mut chunks = Vec::new();
        let mut current_prefix = None;
        let mut output_words = vec![0u64; BITMAP_WORDS];
        let mut cardinality = 0u32;
        for (prefix, container) in self.chunks() {
            let output_prefix = prefix / n as u64;
            if current_prefix != Some(output_prefix) {
                if let Some(old_prefix) = current_prefix {
                    if cardinality != 0 {
                        let words = std::mem::replace(&mut output_words, vec![0; BITMAP_WORDS]);
                        chunks.push((
                            old_prefix,
                            Container::Bitmap(BitmapContainer::from_words(words, cardinality)),
                        ));
                    }
                }
                current_prefix = Some(output_prefix);
                cardinality = 0;
            }
            let words = crate::unstable_arrow::bitmap_words(container)
                .expect("the preflight checked every bitmap payload");
            let output_base = (prefix % n as u64) as usize * (BITMAP_WORDS / n);
            let output_range = output_base..output_base + BITMAP_WORDS / n;
            if fold_words_simd(words, &mut output_words[output_range.clone()], n, reduce) {
                cardinality += output_words[output_range]
                    .iter()
                    .map(|word| word.count_ones())
                    .sum::<u32>();
                continue;
            }
            for (word_index, &word) in words.iter().enumerate() {
                let mut folded = 0u64;
                for (byte_index, byte) in word.to_le_bytes().into_iter().enumerate() {
                    folded |= u64::from(table[byte as usize]) << (byte_index * (8 / n));
                }
                let output_index = output_base + word_index / n;
                let shift = (word_index % n) * (64 / n);
                output_words[output_index] |= folded << shift;
                cardinality += folded.count_ones();
            }
        }
        if let Some(prefix) = current_prefix {
            if cardinality != 0 {
                chunks.push((
                    prefix,
                    Container::Bitmap(BitmapContainer::from_words(output_words, cardinality)),
                ));
            }
        }
        let mut out = OrdSet::from_chunks(chunks);
        out.optimize();
        Some(out)
    }

    /// The interleaved arm: one grouped walk instead of `n` strided filters.
    ///
    /// Correct because under `Interleaved` the physical order **is** the logical
    /// order — `o = x·n + i` is monotone in `x` — so every slot of a logical
    /// ordinal is contiguous and the walk needs only a running count and the
    /// current `x`. That is exactly what fails under `Blocked`, where `x`
    /// restarts at every constituent, and is why this arm declines there rather
    /// than being generalised.
    fn fold_interleaved(&self, v: &View, reduce: Reduce) -> Option<OrdSet> {
        let mut fold = InterleavedFold::new(v, reduce).ok()?;
        for (prefix, container) in self.chunks() {
            fold.push(prefix, container);
        }
        Some(fold.finish())
    }

    /// The inverse image: every constituent's slot, for each logical ordinal here.
    ///
    /// Unlike a fold this is a homomorphism of the whole Boolean signature, so
    /// `a.view_expand( v )` distributes over `∩`, `∪`, `△` and `\` alike. It is
    /// the direction that composes with the rest of the algebra, and
    /// `expand( coarse ).and( fine )` is the query shape it exists for.
    ///
    /// A logical ordinal whose slot is not addressable in some constituent
    /// contributes only the slots that are, rather than being dropped or
    /// erroring — matching [`OrdSet::view_select`], which likewise reports what
    /// is addressable rather than refusing.
    pub fn view_expand(&self, v: &View) -> OrdSet {
        if v.check().is_err() {
            return OrdSet::new();
        }
        if let Some(out) = self.expand_interleaved(v) {
            return out;
        }
        self.expand_generic(v)
    }

    /// Will the interval construction actually pay, on this input?
    ///
    /// **The interval arm is not uniformly better, and shipping it
    /// unconditionally was a real regression.** Measured on 200 000 logical
    /// ordinals at `sets = 4`:
    ///
    /// ```text
    ///                 interval arm   per ordinal
    ///   contiguous        2.01 ms       6.89 ms    arm 3.4x faster
    ///   every 7th         5.92 ms      0.652 ms    arm 9.1x SLOWER
    /// ```
    ///
    /// A contiguous input merges to **one** interval; a scattered one merges to
    /// none, so the builder makes an `insert_range` call per input ordinal while
    /// the generic path makes one bulk sorted build. That is the crate's oldest
    /// measured asymmetry in disguise — bulk build against per-ordinal insert,
    /// 133 µs against 3.70 ms in the README's table.
    ///
    /// So the arm asks first. A counting pre-pass is `O( nnz )` with no
    /// allocation, and both candidate paths are already `O( nnz )`, so it is
    /// bounded by what the work costs anyway.
    ///
    /// [`EXPAND_INTERVAL_COST`] is a **measured cost ratio, not a tuning
    /// knob** — see its rationale before changing it.
    fn expansion_coalesces(&self, sets: u64) -> bool {
        let mut intervals: u64 = 0;
        let mut prev_end: Option<u64> = None;
        for x in self.iter() {
            let Some(lo) = x.checked_mul(sets) else { break };
            if prev_end != Some(lo) {
                intervals += 1;
            }
            prev_end = Some(lo.saturating_add(sets));
        }
        // Emitted slots the generic path would push individually.
        let slots = self.len().saturating_mul(sets);
        intervals.saturating_mul(EXPAND_INTERVAL_COST) <= slots
    }

    /// The oracle: every addressable slot of every logical ordinal.
    ///
    /// **Bounded by [`View::addressable_sets`], not by `sets`.** A descriptor may
    /// legally declare more constituents than the ordinal space can hold, and
    /// looping to `sets` would then visit billions of slots that `ordinal_of`
    /// rejects one at a time — work proportional to the *descriptor* rather than
    /// to the data or the output, which is what turned a 30-byte request into
    /// four billion iterations. `ordinal_of` is monotone in the constituent, so
    /// the addressable ones are a prefix and stopping at its end drops nothing.
    fn expand_generic(&self, v: &View) -> OrdSet {
        let mut sink = crate::pack::OrdinalSink::new();
        for x in self.iter() {
            for i in 0..v.addressable_sets(x) {
                if let Some(o) = v.ordinal_of(i, x) {
                    sink.push(o);
                }
            }
        }
        sink.build()
    }

    /// The interleaved arm: one interval per logical ordinal, and adjacent
    /// logical ordinals produce adjacent intervals.
    ///
    /// Under `Interleaved` a logical ordinal's `n` slots are `[x·n, x·n + n)` —
    /// **contiguous** — so the whole expansion is a run construction rather than
    /// `n` ordinals per input bit. A contiguous input collapses to a single run.
    /// It does not apply to `Blocked`, whose slots are `n` scattered
    /// singletons, and that difference is real rather than an implementation gap.
    fn expand_interleaved(&self, v: &View) -> Option<OrdSet> {
        let ViewLayout::Interleaved = v.layout() else {
            return None;
        };
        let n = v.sets() as u64;
        if !self.expansion_coalesces(n) {
            return None;
        }
        let mut b = IntervalBuilder::default();
        let mut pending: Option<(u64, u64)> = None;
        for x in self.iter() {
            let Some(lo) = x.checked_mul(n) else { break };
            if lo > ORDINAL_MAX {
                break;
            }
            // Clip the top slot group to the universe rather than dropping it.
            let hi = lo.saturating_add(n).min(ORDINAL_MAX.saturating_add(1));
            pending = Some(match pending {
                Some((plo, phi)) if phi == lo => (plo, hi),
                Some((plo, phi)) => {
                    b.add(plo, phi);
                    (lo, hi)
                }
                None => (lo, hi),
            });
        }
        if let Some((plo, phi)) = pending {
            b.add(plo, phi);
        }
        Some(b.build())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ViewSink;
    use std::collections::BTreeSet;

    #[cfg(all(target_arch = "aarch64", target_endian = "little"))]
    #[test]
    fn neon_folds_match_scalar_at_byte_word_and_chunk_seams() {
        assert!(std::arch::is_aarch64_feature_detected!("neon"));
        let mut one_hot = vec![0u64; BITMAP_WORDS];
        for bit in [0, 3, 4, 7, 8, 31, 63, 64, 127, 128, 511, 65_535] {
            one_hot[bit / 64] |= 1u64 << (bit % 64);
        }
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let dense: Vec<u64> = (0..BITMAP_WORDS)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            })
            .collect();
        for words in [
            vec![0u64; BITMAP_WORDS],
            vec![u64::MAX; BITMAP_WORDS],
            one_hot,
            vec![0x1111_1111_1111_1111; BITMAP_WORDS],
            dense,
        ] {
            for sets in [2usize, 4, 8] {
                for reduce in [Reduce::Any, Reduce::All, Reduce::Parity] {
                    let table = fold_table(sets as u32, reduce).unwrap();
                    let mut scalar = vec![0u64; BITMAP_WORDS / sets];
                    for (i, &word) in words.iter().enumerate() {
                        let mut folded = 0u64;
                        for (byte_index, byte) in word.to_le_bytes().into_iter().enumerate() {
                            folded |= u64::from(table[byte as usize]) << ((8 / sets) * byte_index);
                        }
                        scalar[i / sets] |= folded << ((64 / sets) * (i % sets));
                    }
                    let mut vector = vec![0u64; BITMAP_WORDS / sets];
                    // SAFETY: NEON was detected and both slices satisfy B9.
                    unsafe {
                        match sets {
                            2 => neon::fold2(&words, &mut vector, reduce),
                            4 => neon::fold4(&words, &mut vector, reduce),
                            8 => neon::fold8(&words, &mut vector, reduce),
                            _ => unreachable!(),
                        }
                    }
                    assert_eq!(vector, scalar, "sets={sets} {reduce:?}");
                }
            }
        }
    }

    /// The x86 companion of the NEON fold differential. Same five payloads,
    /// all three arities and all three reductions, against the same scalar
    /// byte-table oracle, so the two arms are checked against one definition
    /// rather than against each other.
    #[cfg(all(target_arch = "x86_64", target_endian = "little"))]
    #[test]
    fn sse_folds_match_scalar_at_byte_word_and_chunk_seams() {
        // Asserted, not skipped. SSSE3 predates every x86_64 CPU this project
        // targets, and a test that returns early on a missing feature is
        // indistinguishable in the log from one that ran -- the degradation
        // `ops::bitmap` records as staying "green for a kernel it never
        // executed".
        assert!(std::arch::is_x86_feature_detected!("ssse3"));
        let mut one_hot = vec![0u64; BITMAP_WORDS];
        for bit in [0, 3, 4, 7, 8, 31, 63, 64, 127, 128, 511, 512, 65_535] {
            one_hot[bit / 64] |= 1u64 << (bit % 64);
        }
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let dense: Vec<u64> = (0..BITMAP_WORDS)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                state
            })
            .collect();
        for words in [
            vec![0u64; BITMAP_WORDS],
            vec![u64::MAX; BITMAP_WORDS],
            one_hot,
            vec![0x1111_1111_1111_1111; BITMAP_WORDS],
            vec![0x8000_0000_0000_0001; BITMAP_WORDS],
            dense,
        ] {
            for sets in [2usize, 4, 8] {
                for reduce in [Reduce::Any, Reduce::All, Reduce::Parity] {
                    let table = fold_table(sets as u32, reduce).unwrap();
                    let mut scalar = vec![0u64; BITMAP_WORDS / sets];
                    for (i, &word) in words.iter().enumerate() {
                        let mut folded = 0u64;
                        for (byte_index, byte) in word.to_le_bytes().into_iter().enumerate() {
                            folded |= u64::from(table[byte as usize]) << ((8 / sets) * byte_index);
                        }
                        scalar[i / sets] |= folded << ((64 / sets) * (i % sets));
                    }
                    let mut vector = vec![0u64; BITMAP_WORDS / sets];
                    // SAFETY: SSSE3 was detected and both slices satisfy B9.
                    unsafe {
                        match sets {
                            2 => sse::fold2(&words, &mut vector, reduce),
                            4 => sse::fold4(&words, &mut vector, reduce),
                            8 => sse::fold8(&words, &mut vector, reduce),
                            _ => unreachable!(),
                        }
                    }
                    assert_eq!(vector, scalar, "sets={sets} {reduce:?}");
                }
            }
        }
    }

    fn as_btree(s: &OrdSet) -> BTreeSet<u64> {
        s.iter().collect()
    }

    fn parts() -> Vec<OrdSet> {
        vec![
            OrdSet::from_iter_unsorted([0u64, 1, 2, 5, 900]),
            OrdSet::from_iter_unsorted([1u64, 2, 3, 900]),
            OrdSet::from_iter_unsorted((0..600u64).map(|i| i * 3)),
        ]
    }

    fn pack(v: View, ps: &[OrdSet]) -> OrdSet {
        let mut s = ViewSink::new(v);
        for (i, p) in ps.iter().enumerate() {
            s.place(i as u32, p).unwrap();
        }
        s.build()
    }

    /// The blocked strides must exceed the fixture's largest logical ordinal
    /// ( 1 797, from the multiples-of-three constituent ), or `place` refuses it
    /// as out of the constituent's capacity. An earlier version used 1 000 and
    /// failed there — the descriptor's capacity is a real constraint on the
    /// data, not a formality.
    fn views() -> Vec<View> {
        vec![
            View::interleaved(3),
            View::blocked(3, 2000),
            View::blocked(3, 65_536),
        ]
    }

    /// The fold means what its name says, checked against an oracle built from
    /// the constituents rather than from the packed form.
    #[test]
    fn fold_agrees_with_combining_the_constituents() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            let union = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.or(b));
            let inter = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.and(b));
            let sym = ps.iter().skip(1).fold(ps[0].clone(), |a, b| a.xor(b));

            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::Any)),
                as_btree(&union),
                "{v:?} any"
            );
            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::All)),
                as_btree(&inter),
                "{v:?} all"
            );
            assert_eq!(
                as_btree(&packed.view_fold(&v, Reduce::Parity)),
                as_btree(&sym),
                "{v:?} parity"
            );
            assert!(!inter.is_empty(), "the fixture must exercise a non-empty ∩");
        }
    }

    /// The specialised arm and the oracle are two total functions over the
    /// same domain; this is the diff that keeps them honest.
    #[test]
    fn the_interleaved_arm_agrees_with_the_select_oracle() {
        let ps = parts();
        let mut reached = 0u32;
        let mut nonempty = 0u32;
        for sets in [1u32, 2, 3] {
            let v = View::interleaved(sets);
            let packed = pack(v, &ps[..sets as usize]);
            for r in [Reduce::Any, Reduce::All, Reduce::Parity] {
                let want = packed.fold_via_select(&v, r);
                let got = packed.fold_interleaved(&v, r).expect("arm applies");
                reached += 1;
                nonempty += u32::from(!got.is_empty());
                assert_eq!(as_btree(&got), as_btree(&want), "sets={sets} {r:?}");
            }
        }
        assert!(reached >= 9, "the arm fired only {reached} times");
        assert!(nonempty > 5, "only {nonempty} non-empty results");
        // And it declines where it must.
        let b = View::blocked(2, 100);
        assert!(OrdSet::new().fold_interleaved(&b, Reduce::Any).is_none());
    }

    /// `All` is `Any`'s De Morgan dual. This is the law most likely to catch a
    /// miscount, because it relates the two arms through a complement.
    #[test]
    fn all_is_the_de_morgan_dual_of_any() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            // Complement each constituent within a window, repack, and fold.
            let hi = 1000u64;
            let flipped: Vec<OrdSet> = ps.iter().map(|p| p.not_in_range(0, hi)).collect();
            let packed_not = pack(v, &flipped);

            let all_of_not = packed_not.view_fold(&v, Reduce::All);
            let not_any = packed.view_fold(&v, Reduce::Any).not_in_range(0, hi);
            assert_eq!(as_btree(&all_of_not), as_btree(&not_any), "{v:?}");
        }
    }

    /// The one-sided law, and the assertion that it is **sometimes strict** —
    /// without that second half the test passes on an implementation that is
    /// accidentally exact, which is the failure mode §11 warns about.
    #[test]
    fn fold_of_an_intersection_is_contained_and_sometimes_strictly() {
        let v = View::interleaved(2);
        let a = pack(
            v,
            &[
                OrdSet::from_iter_unsorted([0u64, 1]),
                OrdSet::from_iter_unsorted([2u64]),
            ],
        );
        let b = pack(
            v,
            &[
                OrdSet::from_iter_unsorted([2u64]),
                OrdSet::from_iter_unsorted([0u64, 1]),
            ],
        );
        let lhs = a.and(&b).view_fold(&v, Reduce::Any);
        let rhs = a
            .view_fold(&v, Reduce::Any)
            .and(&b.view_fold(&v, Reduce::Any));
        for x in lhs.iter() {
            assert!(rhs.contains(x), "⊆ fails at {x}");
        }
        assert!(
            lhs.len() < rhs.len(),
            "the inclusion must be strict here, or the test proves nothing"
        );
    }

    /// The Galois connection, which pins the two directions against each other.
    #[test]
    fn expand_and_fold_form_an_adjunction() {
        let ps = parts();
        for v in views() {
            let packed = pack(v, &ps);
            // S ⊆ expand( fold( S ) ) — the round trip only grows.
            let back = packed.view_fold(&v, Reduce::Any).view_expand(&v);
            for o in packed.iter() {
                assert!(back.contains(o), "{v:?} lost physical {o}");
            }
            // fold( expand( C ) ) == C — expanding then folding is the identity,
            // because every constituent gets the same bit.
            let coarse = OrdSet::from_iter_unsorted([0u64, 3, 4, 900]);
            let round = coarse.view_expand(&v).view_fold(&v, Reduce::Any);
            assert_eq!(as_btree(&round), as_btree(&coarse), "{v:?}");
            // And with every constituent set, `All` agrees too.
            let round_all = coarse.view_expand(&v).view_fold(&v, Reduce::All);
            assert_eq!(as_btree(&round_all), as_btree(&coarse), "{v:?} all");
        }
    }

    /// Expansion distributes over the whole signature — the property a fold
    /// does not have, and the reason this is the composable direction.
    #[test]
    fn expand_is_a_homomorphism_of_every_boolean_operator() {
        let v = View::interleaved(3);
        let a = OrdSet::from_iter_unsorted([0u64, 1, 5, 900]);
        let b = OrdSet::from_iter_unsorted([1u64, 2, 900]);
        for (name, want, got) in [
            (
                "and",
                a.and(&b).view_expand(&v),
                a.view_expand(&v).and(&b.view_expand(&v)),
            ),
            (
                "or",
                a.or(&b).view_expand(&v),
                a.view_expand(&v).or(&b.view_expand(&v)),
            ),
            (
                "xor",
                a.xor(&b).view_expand(&v),
                a.view_expand(&v).xor(&b.view_expand(&v)),
            ),
            (
                "andnot",
                a.and_not(&b).view_expand(&v),
                a.view_expand(&v).and_not(&b.view_expand(&v)),
            ),
        ] {
            assert_eq!(as_btree(&want), as_btree(&got), "{name}");
        }
    }

    /// Whichever arm answers, the result is the same — and the public entry
    /// point agrees with the generic path on every input, applied or declined.
    #[test]
    fn expand_agrees_whichever_arm_answers() {
        let v = View::interleaved(4);
        let mut applied = 0u32;
        let mut declined = 0u32;
        for src in [
            OrdSet::new(),
            OrdSet::from_iter_unsorted([0u64]),
            OrdSet::from_iter_unsorted([0u64, 1, 2, 3, 100, 65_535, 65_536]),
            OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 7)),
            OrdSet::from_iter_unsorted(0..20_000u64),
        ] {
            let want = src.expand_generic(&v);
            match src.expand_interleaved(&v) {
                Some(got) => {
                    applied += 1;
                    assert_eq!(as_btree(&got), as_btree(&want));
                }
                None => declined += 1,
            }
            // The public entry point is correct either way.
            let out = src.view_expand(&v);
            assert_eq!(as_btree(&out), as_btree(&want));
            assert_eq!(out.len(), src.len() * 4);
        }
        // Both branches must be reached, or the dispatch is untested.
        assert!(applied > 0, "the interval arm never applied");
        assert!(declined > 0, "the interval arm never declined");
    }

    /// **The dispatch itself**, which exists because the interval arm is
    /// *slower* on scattered input — 9.1x slower, measured. A contiguous range
    /// must take it and collapse to runs; an every-seventh input must not.
    #[test]
    fn the_interval_arm_is_taken_only_when_the_expansion_coalesces() {
        let v = View::interleaved(4);

        let dense = OrdSet::from_iter_unsorted(0..20_000u64);
        assert!(dense.expansion_coalesces(4), "one interval must qualify");
        let e = dense.view_expand(&v);
        assert_eq!(e.chunk_count(), 2, "80 000 bits is two chunks");
        for (_, c) in e.chunks() {
            assert_eq!(c.kind(), crate::ContainerKind::Run, "must coalesce to runs");
        }

        // Every seventh logical ordinal: no two slot groups are adjacent, so the
        // builder would make one `insert_range` per input ordinal.
        let scattered = OrdSet::from_iter_unsorted((0..5000u64).map(|i| i * 7));
        assert!(
            !scattered.expansion_coalesces(4),
            "a non-coalescing input must decline"
        );
        assert!(scattered.expand_interleaved(&v).is_none());
    }

    /// **The amplification regression.** `sets` is a `u32` a caller may declare
    /// far larger than any data justifies, and both loops used to run to it: a
    /// set holding two ordinals drove four billion `view_select` calls. The
    /// bound is `occupied_sets`, and the assertion that matters is not the
    /// value — it is that this test *returns*. If the bound regresses the test
    /// does not fail, it hangs, and CI reports the timeout.
    #[test]
    fn a_fold_costs_what_the_data_holds_not_what_the_descriptor_declares() {
        let v = View::blocked(u32::MAX, 1);
        // Stride 1 gives each constituent exactly logical ordinal 0, so this is
        // constituents 0 and 5 holding `{0}` and every other one empty.
        let s = OrdSet::from_iter_unsorted([0u64, 5]);
        assert_eq!(occupied_sets(&s, &v), 6, "bounded by the maximum ordinal");

        assert_eq!(
            as_btree(&s.view_fold(&v, Reduce::Any)),
            as_btree(&OrdSet::from_iter_unsorted([0u64]))
        );
        // Truncating the loop must not turn `All` into a union of what was
        // visited: constituent 1 is empty, so the intersection is empty.
        assert!(s.view_fold(&v, Reduce::All).is_empty());
        // Two constituents hold logical 0, so the parity is even.
        assert!(s.view_fold(&v, Reduce::Parity).is_empty());
    }

    /// The same property for expansion, which was the worse of the two — its
    /// inner loop ran per *input ordinal*, so the cost was cardinality x sets.
    ///
    /// **This is a wall-clock assertion, and it has to be.** The bounded and
    /// unbounded loops return byte-identical results — `ordinal_of` rejects the
    /// extra constituents one at a time — so no output, cardinality or
    /// allocation count can tell them apart. Time is the only observable, which
    /// is what makes the defect a defect.
    ///
    /// Sabotage-verified 2026-09-19: reverting the bound to `0..v.sets()` left
    /// this test **passing in 57.88 s** at one input ordinal, which is why the
    /// fixture carries four and asserts a deadline. Four ordinals put the
    /// unbounded path near 230 s in debug; the bounded path does 4 x 256 = 1024
    /// iterations and finishes in microseconds. The 10 s deadline therefore has
    /// roughly a 20x margin below the broken cost even in release, and about six
    /// orders of magnitude above the correct one.
    #[test]
    fn an_expansion_visits_only_addressable_constituents() {
        let stride = 1u64 << 56;
        let v = View::blocked(u32::MAX, stride);
        // `i * 2^56` leaves the ordinal universe after 256 constituents
        // ( `ORDINAL_MAX` is `u64::MAX - 1`, not `2^48` -- that is the *prefix*
        // width ), so the other four billion are unaddressable and must never
        // be visited.
        assert_eq!(v.addressable_sets(0), 256);

        let src = OrdSet::from_iter_unsorted([0u64, 1, 2, 3]);
        let t = std::time::Instant::now();
        let out = src.view_expand(&v);
        let elapsed = t.elapsed();

        assert_eq!(out.len(), 4 * 256, "one slot per addressable constituent");
        assert!(out.contains(0) && out.contains(255 * stride + 3));
        // The next slot would be `256 * stride`, which is `2^64` -- not
        // representable, which is exactly why the bound stops here.
        assert_eq!(v.ordinal_of(256, 0), None);
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "expansion took {elapsed:?}; the loop is running to `sets` again"
        );
    }

    /// `addressable_sets` is a prefix bound, and the prefix is exactly the
    /// constituents `ordinal_of` answers for. Checked against it directly,
    /// because a bound that disagreed would silently drop slots.
    #[test]
    fn addressable_sets_agrees_with_ordinal_of() {
        let cases = [
            (View::interleaved(8), 0u64),
            (View::interleaved(8), 5),
            (View::interleaved(8), ORDINAL_MAX),
            (View::blocked(8, 100), 0),
            (View::blocked(8, 100), 99),
            (View::blocked(8, 100), 100),
            (View::blocked(u32::MAX, 1 << 40), 0),
            (View::interleaved(u32::MAX), ORDINAL_MAX),
        ];
        for (v, x) in cases {
            let k = v.addressable_sets(x);
            // Everything below `k` addresses `x` ..
            for i in [0u32, k.saturating_sub(1)] {
                if i < k {
                    assert!(v.ordinal_of(i, x).is_some(), "{v:?} x={x} i={i}");
                }
            }
            // .. and `k` itself does not, when there is one to test.
            if k < v.sets() {
                assert!(v.ordinal_of(k, x).is_none(), "{v:?} x={x} k={k}");
            }
        }
    }

    /// The bound must not save work the caller actually asked for. Under
    /// `Interleaved` at `x = 0` every declared constituent *is* addressable, so
    /// the honest answer is a huge one and `addressable_sets` says so. This is
    /// the case the wire's `MAX_VIEW_SETS` exists for, and the reason the cap
    /// and this bound are both needed rather than either alone.
    #[test]
    fn an_addressable_expansion_is_not_truncated() {
        let v = View::interleaved(1 << 20);
        assert_eq!(v.addressable_sets(0), 1 << 20, "all of them are reachable");
    }

    #[test]
    fn a_degenerate_view_folds_and_expands_to_nothing() {
        let bad = View::interleaved(0);
        let s = OrdSet::from_iter_unsorted([1u64, 2]);
        assert!(s.view_fold(&bad, Reduce::Any).is_empty());
        assert!(s.view_expand(&bad).is_empty());
    }
}
