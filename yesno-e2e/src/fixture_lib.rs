//! Bazel host for external-database fixtures.
//!
//! This is not a backend variant: PostgreSQL and MySQL link the same world,
//! verb registry, and Monty runner. The Cargo library embeds that same fixture
//! module in its larger ordinary world.

pub mod convert;
pub mod fixture;
pub mod world {
    include!("fixture_world.rs");
}

include!("runner_common.rs");
