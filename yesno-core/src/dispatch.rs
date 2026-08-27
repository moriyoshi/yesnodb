//! Bring your own executor for a commit's per-shard work.
//!
//! # Why the executor is supplied rather than created
//!
//! `yesno-core` is embedded in somebody else's process, and the hosts that
//! would benefit most already run one: `yesno-server` and `yesno-flight` have a
//! tokio runtime sized for the machine they are on. A pool owned by this crate
//! would spawn threads that host never asked for and cannot size. So this crate
//! spawns nothing, ever; [`Sequential`] is the default and runs every task on
//! the calling thread, which is exactly what every embedder gets today.
//!
//! # What it is for, and the numbers that justify it
//!
//! `WriteBatch::commit` has two per-shard phases whose parts are independent:
//! it resolves the chunks a batch needs ( outside every lock ), then applies
//! them ( holding each shard's memtable lock in turn ). Measured 2026-09-17 at
//! 20 000 resident keys, 200 rows per commit spread across every shard:
//!
//! ```text
//!   shards   per-shard hold   SUM per commit   prefetch ( outside locks )
//!        1         97.89 us         97.89 us                   293.68 us
//!        8         13.48 us        107.87 us                   254.43 us
//!       32          3.11 us         99.37 us                   275.47 us
//! ```
//!
//! **The exclusive sum is invariant to shard count.** The holds are taken one
//! shard at a time, so a reader scanning every shard waits the *sum*; running
//! them concurrently makes it wait the *max*. That is the reader-facing win.
//!
//! It only pays with an executor cheap enough. Spawning per commit is not:
//! `std::thread::scope` measured **119.65 us for eight threads, 570.11 us for
//! thirty-two**, against roughly 100 us of exclusive time to save. A persistent
//! pool measured **14.29 us and 54.02 us** for the same fan-out. Both are
//! floors, taken on an idle machine, so a loaded one is worse.
//!
//! # The contract
//!
//! An implementation **must call `f( i )` exactly once for every `i` in `0..n`
//! and must not return until all of them have finished.**
//!
//! Returning early is not expressible, which is worth knowing because it is the
//! dangerous half: `f` is borrowed rather than `'static`, so it cannot be moved
//! into a detached thread, and only a scoped or blocking executor compiles.
//! What an implementation *can* get wrong is **skipping** a task -- memory-safe,
//! and a silent correctness catastrophe, because a commit would drop a shard's
//! work. `WriteBatch::commit` therefore counts completions and refuses the
//! commit if any task did not run, rather than trusting the implementation.
//!
//! Tasks touch disjoint state: task `i` works on one shard, and each shard owns
//! its own memtable and store locks. No ordering between them is required.

use std::sync::Arc;

/// Somewhere to run a commit's per-shard work.
///
/// See the module documentation for the contract. In short: call `f( i )` once
/// for every `i` below `n`, in any order, and do not return until all are done.
pub trait Dispatch: Send + Sync {
    /// Run `f( i )` for every `i` in `0..n`, possibly concurrently.
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync));
}

/// Runs each task in turn on the calling thread. The default.
///
/// Spawns nothing and allocates nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sequential;

impl Dispatch for Sequential {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        for i in 0..n {
            f(i);
        }
    }
}

/// A handle to an executor, cheap to clone.
///
/// # Why a handle rather than `Option<Arc<dyn Dispatch>>`
///
/// A bare `Option<Arc<dyn Dispatch>>` puts two implementation choices -- the
/// `Arc` and the trait object -- into a public field, and makes every internal
/// use site unwrap the `Option` and write the sequential case inline as the
/// `None` arm. After the second such site that is factored into a wrapper that
/// forwards, which is this type; the only question was whether it is public,
/// and making it public is what lets a host name what it passes. `None` meaning
/// "sequential" is also a second way to say what [`Sequential`] already says.
#[derive(Clone)]
pub struct Dispatcher(Arc<dyn Dispatch>);

impl Dispatcher {
    /// Wrap an executor.
    pub fn new<D: Dispatch + 'static>(d: D) -> Self {
        Self(Arc::new(d))
    }

    /// Wrap an executor already behind an `Arc`, sharing it rather than
    /// re-boxing -- a host with one pool serving several databases hands each
    /// a clone of the same `Arc`.
    pub fn from_arc(d: Arc<dyn Dispatch>) -> Self {
        Self(d)
    }

    /// The default: every task on the calling thread, nothing spawned.
    pub fn sequential() -> Self {
        Self::new(Sequential)
    }
}

impl From<Arc<dyn Dispatch>> for Dispatcher {
    fn from(d: Arc<dyn Dispatch>) -> Self {
        Self(d)
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::sequential()
    }
}

impl Dispatch for Dispatcher {
    fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
        self.0.run(n, f)
    }
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dispatcher(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn sequential_runs_every_task_in_order() {
        let seen = std::sync::Mutex::new(Vec::new());
        Sequential.run(5, &|i| seen.lock().unwrap().push(i));
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn an_empty_range_runs_nothing() {
        let hits = AtomicUsize::new(0);
        Sequential.run(0, &|_| {
            hits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(hits.load(Ordering::Relaxed), 0);
        Dispatcher::default().run(0, &|_| {
            hits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(hits.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_handle_forwards_to_what_it_wraps() {
        let hits = AtomicUsize::new(0);
        struct Doubling;
        impl Dispatch for Doubling {
            fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
                for i in 0..n {
                    f(i);
                    f(i);
                }
            }
        }
        // Deliberately a *wrong* executor: the handle must not quietly
        // substitute its own behaviour for the one it was given.
        Dispatcher::new(Doubling).run(3, &|_| {
            hits.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(
            hits.load(Ordering::Relaxed),
            6,
            "the handle did not forward"
        );
    }

    /// A scoped-thread executor, as a host would write one.
    ///
    /// Not shipped publicly: it spawns per call, the mechanism measured at
    /// 119.65 us for eight threads and rejected above. It exists to prove the
    /// trait is implementable by something genuinely concurrent.
    struct Scoped;
    impl Dispatch for Scoped {
        fn run(&self, n: usize, f: &(dyn Fn(usize) + Sync)) {
            std::thread::scope(|s| {
                for i in 0..n {
                    s.spawn(move || f(i));
                }
            });
        }
    }

    #[test]
    fn a_concurrent_executor_runs_every_task_exactly_once() {
        const N: usize = 64;
        let counts: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
        Dispatcher::new(Scoped).run(N, &|i| {
            counts[i].fetch_add(1, Ordering::Relaxed);
        });
        for (i, c) in counts.iter().enumerate() {
            assert_eq!(
                c.load(Ordering::Relaxed),
                1,
                "task {i} ran {} times",
                c.load(Ordering::Relaxed)
            );
        }
    }
}
