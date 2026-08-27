//! Run-length container: sorted, non-overlapping, non-adjacent intervals.
//!
//! In memory the payload holds only the `(start, len_minus_1)` pairs; the
//! leading `nruns` u16 that the Roaring spec mandates is added by the codec on
//! serialize and stripped on parse. `len_minus_1` rather than `end` is what
//! makes the on-disk bytes identical to the spec.

use crate::buffer::U16Store;
use crate::CHUNK_CARD;

/// Invariant: intervals are sorted, non-overlapping, and **non-adjacent**
/// (`[0,3],[4,7]` must be coalesced to `[0,7]`), and `len` is their total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunContainer {
    /// Flat `[start0, lenm1_0, start1, lenm1_1, ...]`.
    pub(crate) runs: U16Store,
    pub(crate) len: u32,
}

impl RunContainer {
    pub fn new() -> Self {
        RunContainer {
            runs: U16Store::from_vec(Vec::new()),
            len: 0,
        }
    }

    /// Build from `(start, end)` inclusive pairs, already sorted and coalesced.
    pub fn from_pairs(pairs: &[(u16, u16)]) -> Self {
        let mut flat = Vec::with_capacity(pairs.len() * 2);
        let mut len = 0u32;
        for &(s, e) in pairs {
            debug_assert!(s <= e);
            flat.push(s);
            flat.push(e - s);
            len += (e - s) as u32 + 1;
        }
        RunContainer {
            runs: U16Store::from_vec(flat),
            len,
        }
    }

    pub(crate) fn from_store(runs: U16Store, len: u32) -> Self {
        RunContainer { runs, len }
    }

    #[inline]
    pub fn nruns(&self) -> u32 {
        (self.runs.len() / 2) as u32
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len == CHUNK_CARD
    }

    /// Flat `u16` payload without the `nruns` prefix.
    #[inline]
    pub fn as_flat(&self) -> &[u16] {
        self.runs.as_slice()
    }

    #[inline]
    pub fn start(&self, i: u32) -> u16 {
        self.runs.as_slice()[i as usize * 2]
    }

    #[inline]
    pub fn end(&self, i: u32) -> u16 {
        let s = self.runs.as_slice();
        let i = i as usize * 2;
        s[i] + s[i + 1]
    }

    /// `(start, end)` inclusive pairs.
    pub fn pairs(&self) -> Vec<(u16, u16)> {
        (0..self.nruns())
            .map(|i| (self.start(i), self.end(i)))
            .collect()
    }

    pub fn contains(&self, v: u16) -> bool {
        // Find the last run whose start is <= v.
        let n = self.nruns();
        if n == 0 {
            return false;
        }
        let mut lo = 0i64;
        let mut hi = n as i64 - 1;
        let mut found = -1i64;
        while lo <= hi {
            let mid = (lo + hi) / 2;
            if self.start(mid as u32) <= v {
                found = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        found >= 0 && v <= self.end(found as u32)
    }

    #[inline]
    pub fn min(&self) -> Option<u16> {
        (self.nruns() > 0).then(|| self.start(0))
    }

    #[inline]
    pub fn max(&self) -> Option<u16> {
        let n = self.nruns();
        (n > 0).then(|| self.end(n - 1))
    }

    pub fn rank(&self, v: u16) -> u32 {
        let mut n = 0u32;
        for i in 0..self.nruns() {
            let (s, e) = (self.start(i), self.end(i));
            if v <= s {
                break;
            }
            n += if v > e {
                (e - s) as u32 + 1
            } else {
                (v - s) as u32
            };
        }
        n
    }

    pub fn select(&self, mut n: u32) -> Option<u16> {
        for i in 0..self.nruns() {
            let (s, e) = (self.start(i), self.end(i));
            let c = (e - s) as u32 + 1;
            if n < c {
                return Some(s + n as u16);
            }
            n -= c;
        }
        None
    }

    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        (0..self.nruns()).flat_map(move |i| {
            let (s, e) = (self.start(i), self.end(i));
            (s as u32..=e as u32).map(|v| v as u16)
        })
    }

    /// Rebuild from a sorted value iterator, coalescing adjacent values.
    pub fn from_sorted_values(vals: impl IntoIterator<Item = u16>) -> Self {
        let mut pairs: Vec<(u16, u16)> = Vec::new();
        for v in vals {
            match pairs.last_mut() {
                Some((_, e)) if *e + 1 == v && *e != u16::MAX => *e = v,
                _ => pairs.push((v, v)),
            }
        }
        Self::from_pairs(&pairs)
    }
}

impl Default for RunContainer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_roundtrip_and_length() {
        let r = RunContainer::from_pairs(&[(0, 10), (20, 20), (100, 200)]);
        assert_eq!(r.nruns(), 3);
        assert_eq!(r.len(), 11 + 1 + 101);
        assert_eq!(r.pairs(), vec![(0, 10), (20, 20), (100, 200)]);
    }

    #[test]
    fn contains_respects_gaps() {
        let r = RunContainer::from_pairs(&[(10, 20), (30, 40)]);
        assert!(r.contains(10));
        assert!(r.contains(20));
        assert!(r.contains(35));
        assert!(!r.contains(9));
        assert!(!r.contains(21));
        assert!(!r.contains(29));
        assert!(!r.contains(41));
    }

    #[test]
    fn min_max_rank_select_agree_with_iter() {
        let r = RunContainer::from_pairs(&[(5, 7), (100, 102)]);
        let vals: Vec<u16> = r.iter().collect();
        assert_eq!(vals, vec![5, 6, 7, 100, 101, 102]);
        assert_eq!(r.min(), Some(5));
        assert_eq!(r.max(), Some(102));
        for (i, &v) in vals.iter().enumerate() {
            assert_eq!(r.select(i as u32), Some(v));
            assert_eq!(r.rank(v), i as u32);
        }
        assert_eq!(r.select(vals.len() as u32), None);
    }

    #[test]
    fn from_sorted_values_coalesces() {
        let r = RunContainer::from_sorted_values([1u16, 2, 3, 7, 8, 20]);
        assert_eq!(r.pairs(), vec![(1, 3), (7, 8), (20, 20)]);
        assert_eq!(r.len(), 6);
    }

    #[test]
    fn full_chunk_is_a_single_run() {
        let r = RunContainer::from_pairs(&[(0, 65535)]);
        assert!(r.is_full());
        assert_eq!(r.len(), 65536);
        assert_eq!(r.nruns(), 1);
    }

    #[test]
    fn run_at_u16_max_does_not_overflow() {
        let r = RunContainer::from_sorted_values([65534u16, 65535]);
        assert_eq!(r.pairs(), vec![(65534, 65535)]);
        assert_eq!(r.len(), 2);
    }
}
