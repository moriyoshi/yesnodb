//! The yesnod plugin channel, peer side: the wire format and a client.
//!
//! Split out of `yesno-plugin` on 2026-10-08 for one reason: **a peer must be
//! linkable into a process that must not contain a storage engine.**
//! `yesno-pg` says so explicitly -- its Flight client is compiled without the
//! server feature so that a PostgreSQL backend does not link `yesno-core` --
//! and a channel transport for that extension needs the client without
//! dragging the engine in behind it.
//!
//! The division is by *who needs a database*, which turned out to be a clean
//! line rather than a negotiated one. The protocol ( [`ipc`] ), the client
//! ( [`client`] ) and the status vocabulary ( [`abi`] ) reference `yesno-core`
//! for exactly one thing between them -- the bitmap word count, a constant --
//! while the host session engine reads a live `Db` on nearly every line. The
//! host half stays in `yesno-plugin`, which re-exports these three modules so
//! that every existing path keeps resolving.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod abi;
pub mod client;
pub mod frame;
pub mod ipc;

pub use frame::read_frame;
