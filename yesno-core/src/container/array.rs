//! Sorted, unique `u16` values. At most [`ARRAY_MAX`] of them.

use crate::buffer::U16Store;
use crate::ARRAY_MAX;

/// Invariant: values are strictly ascending and `len() <= ARRAY_MAX`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrayContainer {
    pub(crate) vals: U16Store,
}

impl ArrayContainer {
    pub fn new() -> Self {
        ArrayContainer {
            vals: U16Store::from_vec(Vec::new()),
        }
    }

    /// Build from values already known to be sorted and unique.
    pub fn from_sorted_vec(v: Vec<u16>) -> Self {
        debug_assert!(v.windows(2).all(|w| w[0] < w[1]), "not strictly ascending");
        debug_assert!(v.len() <= ARRAY_MAX);
        ArrayContainer {
            vals: U16Store::from_vec(v),
        }
    }

    pub(crate) fn from_store(vals: U16Store) -> Self {
        ArrayContainer { vals }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u16] {
        self.vals.as_slice()
    }

    #[inline]
    pub fn len(&self) -> u32 {
        self.vals.len() as u32
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.vals.is_empty()
    }

    #[inline]
    pub fn contains(&self, v: u16) -> bool {
        self.as_slice().binary_search(&v).is_ok()
    }

    pub fn insert(&mut self, v: u16) -> bool {
        match self.as_slice().binary_search(&v) {
            Ok(_) => false,
            Err(pos) => {
                self.vals.to_mut().insert(pos, v);
                true
            }
        }
    }

    pub fn remove(&mut self, v: u16) -> bool {
        match self.as_slice().binary_search(&v) {
            Ok(pos) => {
                self.vals.to_mut().remove(pos);
                true
            }
            Err(_) => false,
        }
    }

    #[inline]
    pub fn min(&self) -> Option<u16> {
        self.as_slice().first().copied()
    }

    #[inline]
    pub fn max(&self) -> Option<u16> {
        self.as_slice().last().copied()
    }

    /// Number of values strictly less than `v`.
    #[inline]
    pub fn rank(&self, v: u16) -> u32 {
        self.as_slice().partition_point(|&x| x < v) as u32
    }

    #[inline]
    pub fn select(&self, n: u32) -> Option<u16> {
        self.as_slice().get(n as usize).copied()
    }

    /// Number of maximal runs of consecutive values. Drives run-optimization.
    pub fn run_count(&self) -> u32 {
        let s = self.as_slice();
        if s.is_empty() {
            return 0;
        }
        let mut runs = 1u32;
        for w in s.windows(2) {
            if w[1] != w[0] + 1 {
                runs += 1;
            }
        }
        runs
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.as_slice().iter().copied()
    }
}

impl Default for ArrayContainer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_keeps_sorted_and_unique() {
        let mut a = ArrayContainer::new();
        assert!(a.insert(5));
        assert!(a.insert(1));
        assert!(a.insert(3));
        assert!(!a.insert(3), "duplicate insert must report no change");
        assert_eq!(a.as_slice(), &[1, 3, 5]);
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn remove_reports_change() {
        let mut a = ArrayContainer::from_sorted_vec(vec![1, 3, 5]);
        assert!(a.remove(3));
        assert!(!a.remove(3));
        assert_eq!(a.as_slice(), &[1, 5]);
    }

    #[test]
    fn rank_and_select_agree() {
        let a = ArrayContainer::from_sorted_vec(vec![2, 4, 6, 8]);
        assert_eq!(a.rank(0), 0);
        assert_eq!(a.rank(4), 1);
        assert_eq!(a.rank(5), 2);
        assert_eq!(a.rank(9), 4);
        for i in 0..a.len() {
            let v = a.select(i).unwrap();
            assert_eq!(a.rank(v), i);
        }
        assert_eq!(a.select(4), None);
    }

    #[test]
    fn run_count_counts_maximal_runs() {
        assert_eq!(ArrayContainer::from_sorted_vec(vec![]).run_count(), 0);
        assert_eq!(ArrayContainer::from_sorted_vec(vec![1]).run_count(), 1);
        assert_eq!(
            ArrayContainer::from_sorted_vec(vec![1, 2, 3]).run_count(),
            1
        );
        assert_eq!(
            ArrayContainer::from_sorted_vec(vec![1, 3, 5]).run_count(),
            3
        );
        assert_eq!(
            ArrayContainer::from_sorted_vec(vec![1, 2, 3, 7, 8, 20]).run_count(),
            3
        );
    }
}
