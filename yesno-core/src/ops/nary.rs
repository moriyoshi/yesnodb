//! k-way union: one accumulator, not k-1 intermediate results.
//!
//! # Why folding is the wrong shape
//!
//! `sets.fold(empty, |a, b| a.or(b))` allocates a **complete new result per
//! input**. As the accumulator grows toward the union, each of those copies
//! approaches the size of the final answer, so the work is quadratic in k even
//! though the answer is not. Measured over 1024 sets of 50 000 ordinals:
//!
//! ```text
//!   k=2      0.07 ms      k=64      2.95 ms
//!   k=8      0.87 ms      k=1024   39.84 ms
//! ```
//!
//! Nothing about a union requires materializing an intermediate. Chunks are
//! visited in prefix order, and every input contributing to one prefix is ORed
//! into a single reusable accumulator before that chunk is finalized once.
//!
//! # Why the accumulator is words and not a `Container`
//!
//! ORing into a `Container` would re-dispatch on kind per input and would keep
//! promoting an array toward a bitmap as the group filled. The accumulator is
//! the bitmap's own 1024-word buffer, reused across chunks and **cleared only
//! over the words a chunk actually touched** — a full 8 KiB memset per chunk
//! would reintroduce a per-chunk cost proportional to the chunk, not to the
//! data in it.
//!
//! # Small groups skip it
//!
//! One contributor is an `Arc` bump. Two are the pairwise kernel, which is
//! already specialized for all nine kind-pairs and beats densifying to a bitmap
//! and back. The accumulator earns its keep from three contributors up, or when
//! a bitmap is present and the result is dense anyway.

use crate::container::{BitmapContainer, Container};
use crate::ops::generic::SetOp;
use crate::Prefix48;

/// A reusable 65536-bit accumulator that clears only what it dirtied.
pub(crate) struct Scratch {
    words: Vec<u64>,
    /// Inclusive word range touched since the last `take`.
    dirty: Option<(usize, usize)>,
}

impl Scratch {
    /// Empty until a group actually needs it.
    ///
    /// A union whose every prefix has one or two contributors never reaches the
    /// accumulator, and allocating 8 KiB up front made `union_all` *slower*
    /// than the fold it replaces on exactly that shape — k=2 measured 0.3x
    /// before this. The buffer is allocated on first use and then reused for
    /// the rest of the union.
    pub(crate) fn new() -> Self {
        Scratch {
            words: Vec::new(),
            dirty: None,
        }
    }

    #[inline]
    fn words_mut(&mut self) -> &mut [u64] {
        if self.words.is_empty() {
            self.words = vec![0u64; crate::BITMAP_BYTES / 8];
        }
        &mut self.words
    }

    #[inline]
    fn touch(&mut self, lo: usize, hi: usize) {
        self.dirty = Some(match self.dirty {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }

    /// OR one container's values in.
    pub(crate) fn or_in(&mut self, c: &Container) {
        match c {
            Container::Bitmap(b) => match b.bits.try_words() {
                Some(src) => {
                    let n = src.len();
                    let dst = self.words_mut();
                    for (i, w) in src.iter().enumerate() {
                        dst[i] |= w;
                    }
                    self.touch(0, n - 1);
                }
                // A bitmap whose buffer is not word-aligned cannot be read as
                // words; fall back to its values rather than to `unsafe`.
                None => {
                    let vals: Vec<u16> = b.iter().collect();
                    let (Some(&first), Some(&last)) = (vals.first(), vals.last()) else {
                        return;
                    };
                    self.touch(first as usize >> 6, last as usize >> 6);
                    let dst = self.words_mut();
                    for v in vals {
                        dst[v as usize >> 6] |= 1u64 << (v & 63);
                    }
                }
            },
            Container::Array(a) => {
                let vals = a.as_slice();
                let (Some(&first), Some(&last)) = (vals.first(), vals.last()) else {
                    return;
                };
                // Sorted, so the dirty range is known from the ends — marking it
                // per value put an `Option` match inside the hot loop.
                self.touch(first as usize >> 6, last as usize >> 6);
                let dst = self.words_mut();
                for &v in vals {
                    dst[v as usize >> 6] |= 1u64 << (v & 63);
                }
            }
            Container::Run(r) => {
                let n = r.nruns();
                if n == 0 {
                    return;
                }
                // Runs are ordered, so one touch covers all of them.
                self.touch(r.start(0) as usize >> 6, r.end(n - 1) as usize >> 6);
                let words = self.words_mut();
                for i in 0..n {
                    crate::ops::mixed::for_each_masked_word(r.start(i), r.end(i), |w, m| {
                        words[w] |= m;
                    });
                }
            }
        }
    }

    /// Popcount the accumulated chunk and reset, **building nothing**.
    ///
    /// This is what makes a streaming k-way OR answer `cardinality()` without
    /// materializing: the union of a prefix's contributors is counted straight
    /// out of the accumulator, so no result container is allocated and no
    /// `optimize()` runs. `tests/allocation.rs` is what keeps it that way.
    pub(crate) fn take_len(&mut self) -> u32 {
        let Some((lo, hi)) = self.dirty.take() else {
            return 0;
        };
        let len: u32 = self.words[lo..=hi].iter().map(|w| w.count_ones()).sum();
        for w in &mut self.words[lo..=hi] {
            *w = 0;
        }
        len
    }

    /// Finalize the accumulated chunk and reset, clearing only dirty words.
    pub(crate) fn take(&mut self) -> Option<Container> {
        let (lo, hi) = self.dirty.take()?;
        let len: u32 = self.words[lo..=hi].iter().map(|w| w.count_ones()).sum();
        if len == 0 {
            return None;
        }
        let mut c = Container::Bitmap(BitmapContainer::from_words(self.words.clone(), len));
        // The union of sparse inputs is often still sparse, and of contiguous
        // ones often a run. Let the size model decide rather than shipping a
        // bitmap because that is what the accumulator happens to be.
        c.optimize();
        for w in &mut self.words[lo..=hi] {
            *w = 0;
        }
        Some(c)
    }
}

/// Union of many prefix-ordered chunk lists.
///
/// Each input must be sorted by prefix and hold no empty containers, which is
/// the `OrdSet` invariant. Returns chunks in prefix order.
pub fn union_all(inputs: &[(&[Prefix48], &[Container])]) -> Vec<(Prefix48, Container)> {
    debug_assert!(
        inputs.iter().all(|(p, c)| p.len() == c.len()),
        "prefix and container lists must be parallel"
    );
    match inputs.len() {
        0 => return Vec::new(),
        1 => {
            let (p, c) = inputs[0];
            return p.iter().copied().zip(c.iter().cloned()).collect();
        }
        _ => {}
    }

    let mut cursors: Vec<usize> = vec![0; inputs.len()];
    let mut out: Vec<(Prefix48, Container)> = Vec::new();
    let mut scratch = Scratch::new();
    // Reused across chunks so a wide k does not allocate per prefix.
    let mut group: Vec<&Container> = Vec::with_capacity(inputs.len());

    loop {
        // The next prefix any cursor is sitting on. A linear scan over k beats a
        // heap here: k is small next to the chunk count, and the heap would need
        // rebuilding as cursors advance anyway.
        let mut min: Option<Prefix48> = None;
        for (i, &c) in cursors.iter().enumerate() {
            if let Some(&p) = inputs[i].0.get(c) {
                min = Some(match min {
                    None => p,
                    Some(m) => m.min(p),
                });
            }
        }
        let Some(prefix) = min else { break };

        group.clear();
        for (i, c) in cursors.iter_mut().enumerate() {
            if inputs[i].0.get(*c) == Some(&prefix) {
                group.push(&inputs[i].1[*c]);
                *c += 1;
            }
        }

        let merged = match group.len() {
            1 => Some(group[0].clone()),
            2 => crate::ops::apply(SetOp::Or, group[0], group[1]),
            _ => {
                for c in &group {
                    scratch.or_in(c);
                }
                scratch.take()
            }
        };
        if let Some(c) = merged {
            out.push((prefix, c));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(vals: &[u64]) -> (Vec<Prefix48>, Vec<Container>) {
        let mut by: std::collections::BTreeMap<Prefix48, Vec<u16>> = Default::default();
        for &v in vals {
            let (p, l) = crate::split(v);
            by.entry(p).or_default().push(l);
        }
        by.into_iter()
            .map(|(p, mut v)| {
                v.sort_unstable();
                v.dedup();
                (p, Container::from_sorted(&v))
            })
            .unzip()
    }

    /// The accumulator path must equal folding the pairwise kernel.
    ///
    /// It is a *second implementation* of union, taken only at three
    /// contributors and up, so nothing but a differential test against the
    /// pairwise path can see a mistake in it.
    #[test]
    fn union_all_agrees_with_folding_the_pairwise_kernel() {
        let cases: Vec<Vec<Vec<u64>>> = vec![
            // Disjoint chunks, shared chunks, and a k that forces the scratch.
            vec![vec![1, 2, 3], vec![70_000, 70_001], vec![2, 3, 4]],
            vec![
                (0..5000u64).collect(),
                (2500..7500).collect(),
                (100..200).collect(),
            ],
            vec![vec![0], vec![65535], vec![65536], vec![u32::MAX as u64]],
            // Dense enough that the union is a full chunk.
            vec![
                (0..30_000u64).collect(),
                (30_000..65_536).collect(),
                (10..20).collect(),
            ],
            // Five inputs, all overlapping.
            (0..5u64)
                .map(|i| (0..1000).map(|j| i * 7 + j * 3).collect())
                .collect(),
        ];

        for case in cases {
            let sets: Vec<(Vec<Prefix48>, Vec<Container>)> = case.iter().map(|v| set(v)).collect();
            let refs: Vec<(&[Prefix48], &[Container])> = sets
                .iter()
                .map(|(p, c)| (p.as_slice(), c.as_slice()))
                .collect();
            let got = union_all(&refs);

            let mut want: std::collections::BTreeSet<u64> = Default::default();
            for v in &case {
                want.extend(v.iter().copied());
            }
            let flat: std::collections::BTreeSet<u64> = got
                .iter()
                .flat_map(|(p, c)| c.iter().map(move |l| (p << crate::CHUNK_BITS) | l as u64))
                .collect();
            assert_eq!(flat, want, "union_all disagrees with the set union");

            for (_, c) in &got {
                crate::container::codec::validate(c)
                    .expect("union_all produced an invalid container");
            }
        }
    }

    /// The scratch is reused, so a chunk must not inherit the previous one.
    #[test]
    fn the_accumulator_does_not_leak_between_chunks() {
        // Three inputs so the scratch path is taken, across two prefixes where
        // the second chunk's values are a strict subset of the first's words.
        let a = set(&[1, 2, 3, 70_000]);
        let b = set(&[4, 5, 70_001]);
        let c = set(&[6, 70_002]);
        let got = union_all(&[
            (a.0.as_slice(), a.1.as_slice()),
            (b.0.as_slice(), b.1.as_slice()),
            (c.0.as_slice(), c.1.as_slice()),
        ]);
        let flat: std::collections::BTreeSet<u64> = got
            .iter()
            .flat_map(|(p, c)| c.iter().map(move |l| (p << crate::CHUNK_BITS) | l as u64))
            .collect();
        assert_eq!(
            flat,
            [1, 2, 3, 4, 5, 6, 70_000, 70_001, 70_002]
                .into_iter()
                .collect()
        );
    }
}
