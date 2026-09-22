//! Which chunks live on the device, and what it takes to get there.
//!
//! # The policy, and why it is a counter rather than a cost model
//!
//! A discrete accelerator only pays if a chunk is used several times while it
//! is resident, so the decision is "has this recurred enough to be worth
//! filling". The break-even is unusually clean: a fill costs `D / U` of
//! background bandwidth and each later hit saves `( 1 - C / G )` of a CPU
//! operation, and **both scale linearly in the payload size `D`, so the
//! threshold is independent of how big a chunk is**. Projected across
//! accelerators it lands between two and five observations, which is why this
//! is a small integer and not an arithmetic model.
//!
//! # Three things a simulator sweep established, all of them load-bearing
//!
//! The numbers below come from replaying access streams through this policy's
//! model before any of it was built ( recorded in
//! `LTM/gpu-offload-on-unified-memory.md` ).
//!
//! **Evidence must decay.** Without it, a chunk touched once every ten
//! thousand queries eventually accumulates enough observations to qualify, and
//! a cache full of those is a cache that holds nothing useful. With decay it
//! never qualifies, which is correct.
//!
//! **The threshold and the half-life are one parameter, not two.** At a
//! half-life of 20 000 accesses, `admit_after` of 5 measured 78.2% hits while
//! 10 measured **64.2%** -- *below* admitting on first touch -- because
//! evidence decayed faster than ten observations could accumulate. Quoting a
//! threshold without its window is quoting half a parameter, so [`Policy`]
//! carries both and [`Policy::measured`] sets them together.
//!
//! **The win is in bandwidth, not hit rate.** The same sweep moved the hit
//! rate six points and the fill traffic **ninety-two fold**. On a discrete
//! device behind a bus, that ratio is what decides whether the link can carry
//! the policy at all -- so [`Stats::futile_admissions`] is the number to watch,
//! not the hit rate.
//!
//! # And one thing the sweep got wrong, corrected here
//!
//! The first analysis called admission control free, on the grounds that a bad
//! admission only wastes background bandwidth. That is true of admission and
//! false of the *threshold*: where capacity binds, raising `admit_after` from
//! 1 to 10 **halved** the hit rate, because hot chunks are delayed into a
//! cache that evicts them before they qualify. The counter pays when the
//! device can hold a working set and costs when it cannot, which is why
//! [`Policy::capacity`] and [`Policy::admit_after`] must be chosen together
//! against a measured recurrence distribution -- the one `yesno_core::hotspot`
//! exists to capture.

use std::collections::{BTreeSet, HashMap};

use yesno_core::accel::ChunkId;

/// Index of one device-side chunk buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub usize);

/// What the caller should do with this chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Already on the device. Launch against this slot; upload nothing.
    Resident(Slot),
    /// Not resident and worth filling. Upload the payload into this slot,
    /// then launch. The slot is already accounted for as holding this chunk,
    /// so a caller that fails to upload must say so with
    /// [`Residency::abandon`].
    Admit(Slot),
    /// Not resident and not worth filling. Use the CPU path.
    Decline,
}

/// Admission tunables. See the module header: the three move together.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    /// Device-side chunk buffers. Sized by the caller from its memory budget
    /// divided by the payload size, which for a bitmap container is 8 KiB.
    pub capacity: usize,
    /// Observations required before a chunk is filled. `1` admits on first
    /// sight, which is the fill-on-miss policy and the baseline the sweep
    /// above compares against.
    pub admit_after: u32,
    /// Accesses after which an unadmitted chunk's evidence halves. `None`
    /// keeps evidence forever, which makes a drifting working set look
    /// permanently hot.
    pub half_life: Option<f64>,
}

impl Policy {
    /// The measured defaults: admit after about five observations inside a
    /// window of roughly fifteen thousand accesses.
    ///
    /// `capacity` has no measured default because it is a property of the
    /// device, not of the workload.
    pub fn measured(capacity: usize) -> Policy {
        Policy {
            capacity,
            admit_after: 5,
            half_life: Some(15_000.0),
        }
    }
}

/// Counters worth exporting. Every one of them is a diagnosis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub touches: u64,
    pub hits: u64,
    pub admissions: u64,
    pub evictions: u64,
    /// Chunks filled and then evicted without ever being used again. The
    /// number that indicts a threshold set too low: these are pure bus
    /// traffic bought for nothing.
    pub futile_admissions: u64,
    /// Touches that found the chunk cold and not yet worth filling.
    pub declines: u64,
}

impl Stats {
    pub fn hit_rate(&self) -> f64 {
        if self.touches == 0 {
            return 0.0;
        }
        self.hits as f64 / self.touches as f64
    }

    /// Admissions that never paid, as a fraction of admissions.
    pub fn futile_rate(&self) -> f64 {
        if self.admissions == 0 {
            return 0.0;
        }
        self.futile_admissions as f64 / self.admissions as f64
    }
}

struct Evidence {
    weight: f64,
    tick: u64,
}

struct Entry {
    slot: Slot,
    tick: u64,
    /// Whether this chunk has been touched again since it was filled.
    used: bool,
}

/// Device-slot bookkeeping. Holds no device memory itself.
///
/// Deliberately free of any CUDA type: this is the half of the design that can
/// be tested exhaustively without a GPU, and it is where every policy question
/// lives.
pub struct Residency {
    policy: Policy,
    resident: HashMap<ChunkId, Entry>,
    /// `( last use, chunk )`, least-recently-used first. A sorted index rather
    /// than a scan because capacity is measured in thousands of slots.
    order: BTreeSet<(u64, ChunkId)>,
    evidence: HashMap<ChunkId, Evidence>,
    free: Vec<Slot>,
    tick: u64,
    stats: Stats,
}

impl Residency {
    pub fn new(policy: Policy) -> Residency {
        assert!(policy.capacity > 0, "a zero-slot device holds nothing");
        assert!(
            policy.admit_after >= 1,
            "admission needs at least one sight"
        );
        Residency {
            policy,
            resident: HashMap::with_capacity(policy.capacity),
            order: BTreeSet::new(),
            evidence: HashMap::new(),
            free: (0..policy.capacity).map(Slot).rev().collect(),
            tick: 0,
            stats: Stats::default(),
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn resident_count(&self) -> usize {
        self.resident.len()
    }

    /// Record a use of `chunk` and say what the caller should do.
    pub fn touch(&mut self, chunk: ChunkId) -> Decision {
        self.tick += 1;
        self.stats.touches += 1;

        if let Some(entry) = self.resident.get_mut(&chunk) {
            self.stats.hits += 1;
            self.order.remove(&(entry.tick, chunk));
            entry.tick = self.tick;
            entry.used = true;
            self.order.insert((self.tick, chunk));
            return Decision::Resident(entry.slot);
        }

        if self.observe(chunk) < self.admission_threshold() {
            self.stats.declines += 1;
            return Decision::Decline;
        }
        match self.admit(chunk) {
            Some(slot) => Decision::Admit(slot),
            // Every slot is pinned by something more recently used. Nothing to
            // do but let the CPU have it.
            None => {
                self.stats.declines += 1;
                Decision::Decline
            }
        }
    }

    /// Undo an [`Decision::Admit`] the caller could not fulfil.
    ///
    /// A failed upload must not leave a slot marked as holding a payload it
    /// does not hold: a later [`Decision::Resident`] for it would launch
    /// against uninitialized device memory and return wrong counts silently.
    pub fn abandon(&mut self, chunk: ChunkId) {
        if let Some(entry) = self.resident.remove(&chunk) {
            self.order.remove(&(entry.tick, chunk));
            self.free.push(entry.slot);
            self.stats.admissions = self.stats.admissions.saturating_sub(1);
        }
    }

    /// The weight at which a chunk is worth filling.
    ///
    /// **Half an observation below the nominal threshold, and that is not a
    /// fudge.** Evidence is a *decayed* count, so `admit_after` consecutive
    /// touches never quite sum to `admit_after`: five touches under a 15 000
    /// half-life accumulate 4.9997, because the four earlier observations each
    /// decay by a tick before the fifth arrives. Comparing against the bare
    /// integer therefore means "admit after six", silently, for every
    /// threshold and every half-life -- a policy that does not do what its own
    /// field name says.
    ///
    /// Rounding to the nearest whole observation says what was meant: admit
    /// once the decayed count rounds to `admit_after`. It cannot resurrect a
    /// slow drip, which is decay's actual job -- a chunk touched every 100
    /// accesses under a half-life of 10 sits at a weight of 1 forever, and 1
    /// does not round to 3.
    fn admission_threshold(&self) -> f64 {
        self.policy.admit_after as f64 - 0.5
    }

    /// Decay this chunk's evidence to now, then add one observation.
    fn observe(&mut self, chunk: ChunkId) -> f64 {
        let tick = self.tick;
        let half_life = self.policy.half_life;
        let e = self
            .evidence
            .entry(chunk)
            .or_insert(Evidence { weight: 0.0, tick });
        if let Some(h) = half_life {
            e.weight *= 0.5f64.powf((tick - e.tick) as f64 / h);
        }
        e.tick = tick;
        e.weight += 1.0;
        e.weight
    }

    fn admit(&mut self, chunk: ChunkId) -> Option<Slot> {
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => self.evict()?,
        };
        self.resident.insert(
            chunk,
            Entry {
                slot,
                tick: self.tick,
                used: false,
            },
        );
        self.order.insert((self.tick, chunk));
        // Evidence is spent. Keeping it would let a chunk evicted for being
        // cold walk straight back in on its next touch, which is the classic
        // admission-control thrash.
        self.evidence.remove(&chunk);
        self.stats.admissions += 1;
        Some(slot)
    }

    fn evict(&mut self) -> Option<Slot> {
        let &(tick, victim) = self.order.iter().next()?;
        self.order.remove(&(tick, victim));
        let entry = self
            .resident
            .remove(&victim)
            .expect("order tracks residents");
        self.stats.evictions += 1;
        if !entry.used {
            self.stats.futile_admissions += 1;
        }
        self.evidence.remove(&victim);
        Some(entry.slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(capacity: usize, admit_after: u32) -> Policy {
        Policy {
            capacity,
            admit_after,
            half_life: None,
        }
    }

    fn slots_of(d: Decision) -> Option<Slot> {
        match d {
            Decision::Resident(s) | Decision::Admit(s) => Some(s),
            Decision::Decline => None,
        }
    }

    #[test]
    fn admitting_on_first_sight_is_fill_on_miss() {
        let mut r = Residency::new(policy(4, 1));
        assert!(matches!(r.touch(ChunkId(1)), Decision::Admit(_)));
        assert!(matches!(r.touch(ChunkId(1)), Decision::Resident(_)));
        let s = r.stats();
        assert_eq!((s.admissions, s.hits, s.declines), (1, 1, 0));
    }

    #[test]
    fn exactly_admit_after_touches_admit_even_under_decay() {
        // The regression test for a silent off-by-one: evidence is a decayed
        // count, so five consecutive touches sum to 4.9997 rather than 5, and
        // comparing against the bare integer turned every threshold into
        // `threshold + 1` without saying so.
        for admit_after in 1..=8u32 {
            let mut r = Residency::new(Policy {
                capacity: 4,
                admit_after,
                half_life: Some(15_000.0),
            });
            for i in 1..admit_after {
                assert_eq!(
                    r.touch(ChunkId(1)),
                    Decision::Decline,
                    "admit_after={admit_after} admitted early at touch {i}"
                );
            }
            assert!(
                matches!(r.touch(ChunkId(1)), Decision::Admit(_)),
                "admit_after={admit_after} did not admit on touch {admit_after}"
            );
        }
    }

    #[test]
    fn the_threshold_counts_observations_before_admission() {
        let mut r = Residency::new(policy(4, 3));
        for _ in 0..2 {
            assert_eq!(r.touch(ChunkId(7)), Decision::Decline);
        }
        assert!(matches!(r.touch(ChunkId(7)), Decision::Admit(_)));
        assert!(matches!(r.touch(ChunkId(7)), Decision::Resident(_)));
        assert_eq!(r.stats().declines, 2);
    }

    #[test]
    fn a_chunk_seen_once_never_costs_a_fill() {
        let mut r = Residency::new(policy(8, 5));
        for k in 0..1000 {
            assert_eq!(r.touch(ChunkId(k)), Decision::Decline);
        }
        assert_eq!(r.stats().admissions, 0, "nothing recurred, nothing filled");
        assert_eq!(r.resident_count(), 0);
    }

    #[test]
    fn a_resident_chunk_keeps_its_slot() {
        let mut r = Residency::new(policy(4, 1));
        let first = slots_of(r.touch(ChunkId(3))).expect("admitted");
        for _ in 0..10 {
            assert_eq!(r.touch(ChunkId(3)), Decision::Resident(first));
        }
    }

    #[test]
    fn slots_are_unique_while_held_and_recycled_after_eviction() {
        let mut r = Residency::new(policy(2, 1));
        let a = slots_of(r.touch(ChunkId(1))).expect("a");
        let b = slots_of(r.touch(ChunkId(2))).expect("b");
        assert_ne!(a, b, "two live chunks must not share a buffer");
        // Third chunk evicts the least recently used, and inherits its slot.
        let c = slots_of(r.touch(ChunkId(3))).expect("c");
        assert_eq!(c, a, "the evicted chunk's buffer is reused");
        assert_eq!(r.stats().evictions, 1);
        assert_eq!(r.resident_count(), 2, "capacity is never exceeded");
    }

    #[test]
    fn eviction_takes_the_least_recently_used() {
        let mut r = Residency::new(policy(2, 1));
        r.touch(ChunkId(1));
        r.touch(ChunkId(2));
        r.touch(ChunkId(1)); // 1 is now the more recent of the two
        r.touch(ChunkId(3)); // evicts 2
        assert!(matches!(r.touch(ChunkId(1)), Decision::Resident(_)));
        // Under a threshold of one, every miss admits -- so the proof that 2
        // was the victim is that it has to be *filled again*, not that it is
        // declined. Asserting `Decline` here would be asserting the threshold,
        // not the eviction order.
        assert!(
            matches!(r.touch(ChunkId(2)), Decision::Admit(_)),
            "2 was evicted and must be re-filled rather than found resident"
        );
    }

    #[test]
    fn decay_stops_a_slow_drip_from_ever_qualifying() {
        // One touch of chunk 7 every 100 accesses against a half life of 10:
        // its evidence is down to 2^-10 before the next observation arrives.
        let mut r = Residency::new(Policy {
            capacity: 8,
            admit_after: 3,
            half_life: Some(10.0),
        });
        for round in 0..50u64 {
            r.touch(ChunkId(7));
            for i in 0..99 {
                r.touch(ChunkId(1000 + round * 100 + i));
            }
        }
        assert_eq!(r.stats().admissions, 0);
    }

    #[test]
    fn without_decay_the_same_slow_drip_is_admitted() {
        let mut r = Residency::new(policy(8, 3));
        for round in 0..50u64 {
            r.touch(ChunkId(7));
            for i in 0..99 {
                r.touch(ChunkId(1000 + round * 100 + i));
            }
        }
        assert!(
            r.stats().admissions >= 1,
            "evidence that never decays accumulates forever"
        );
    }

    #[test]
    fn evidence_does_not_survive_eviction() {
        // Otherwise a chunk evicted for being cold walks straight back in on
        // its next touch, and the cache thrashes at exactly the rate the
        // threshold was supposed to prevent.
        let mut r = Residency::new(policy(1, 2));
        r.touch(ChunkId(1));
        r.touch(ChunkId(1)); // admitted, holds the only slot
        r.touch(ChunkId(2));
        r.touch(ChunkId(2)); // admitted, evicts 1
        assert_eq!(r.stats().evictions, 1);
        // Chunk 1 must start over rather than re-enter on one sight.
        assert_eq!(r.touch(ChunkId(1)), Decision::Decline);
    }

    #[test]
    fn futile_admissions_are_counted() {
        // Capacity one, two chunks alternating in pairs: each is filled on its
        // second touch, then evicted by the other before being used again.
        let mut r = Residency::new(policy(1, 2));
        for _ in 0..10 {
            r.touch(ChunkId(1));
            r.touch(ChunkId(1));
            r.touch(ChunkId(2));
            r.touch(ChunkId(2));
        }
        let s = r.stats();
        assert!(s.admissions > 0);
        assert_eq!(s.hits, 0, "every fill is evicted before it pays");
        assert_eq!(s.futile_admissions, s.evictions);
        assert!(
            s.futile_rate() > 0.9,
            "this policy is indicted by its own stats"
        );
    }

    #[test]
    fn abandoning_a_failed_upload_frees_the_slot_and_forgets_the_chunk() {
        // A slot marked as holding a payload it does not hold would later be
        // launched against uninitialized device memory, and return wrong
        // counts with no error anywhere.
        let mut r = Residency::new(policy(1, 1));
        let slot = slots_of(r.touch(ChunkId(1))).expect("admitted");
        r.abandon(ChunkId(1));
        assert_eq!(r.resident_count(), 0);
        assert_eq!(r.stats().admissions, 0, "an abandoned fill did not happen");
        let again = slots_of(r.touch(ChunkId(2))).expect("the slot is free again");
        assert_eq!(again, slot);
    }

    #[test]
    fn abandoning_something_not_admitted_is_harmless() {
        let mut r = Residency::new(policy(2, 5));
        r.touch(ChunkId(1));
        r.abandon(ChunkId(1));
        r.abandon(ChunkId(99));
        assert_eq!(r.resident_count(), 0);
    }

    #[test]
    fn capacity_is_never_exceeded_under_a_hostile_stream() {
        let mut r = Residency::new(policy(16, 2));
        let mut x = 1u64;
        for _ in 0..50_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            r.touch(ChunkId(x >> 40));
            assert!(r.resident_count() <= 16);
        }
        let s = r.stats();
        assert_eq!(s.touches, 50_000);
        assert_eq!(s.hits + s.admissions + s.declines, s.touches);
    }

    #[test]
    fn every_touch_lands_in_exactly_one_outcome() {
        // The stats are an accounting identity, and a decision that fell
        // through without incrementing anything would be invisible otherwise.
        let mut r = Residency::new(policy(4, 3));
        for k in 0..200u64 {
            r.touch(ChunkId(k % 7));
        }
        let s = r.stats();
        assert_eq!(s.hits + s.admissions + s.declines, s.touches);
    }

    #[test]
    fn the_measured_policy_carries_its_window_with_its_threshold() {
        // The sweep found `admit_after` meaningless without a half-life: at
        // 20 000 accesses a threshold of 10 scored *below* admitting on sight.
        let p = Policy::measured(1024);
        assert_eq!(p.admit_after, 5);
        assert!(
            p.half_life.is_some(),
            "a threshold without a window is half a parameter"
        );
    }
}
