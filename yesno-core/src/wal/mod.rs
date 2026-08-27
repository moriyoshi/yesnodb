//! Write-ahead log.
//!
//! The WAL has exactly two jobs: durability of commits between checkpoints, and
//! being the replication stream. It deliberately does **not** protect against
//! torn pages — extents and index nodes are shadow-paged, so a half-written new
//! page is unreachable until a root pointer flip makes it reachable, and the
//! only atomicity requirement in the store is the A/B superblock. No
//! Postgres-style full-page writes are needed here, and re-adding them would be
//! pure cost.

pub mod group;
pub mod record;
pub mod recover;
pub mod writer;

pub use group::{GenerationRoll, GroupCommit};
pub use record::{RecType, Record, Scanner, HEADER};
pub use recover::{plan, RecoveryPlan, ShardLog};
pub use writer::{log_bounds, read_first_frame, read_log_range, remove_log_generations, WalWriter};
