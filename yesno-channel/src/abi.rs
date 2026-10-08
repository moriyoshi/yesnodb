//! Status and role codes, shared by the host and the channel.
//!
//! **These were the plain-data half of an in-process C ABI**, mirrored from
//! `include/yesno_plugin.h` and checked against it by a test that parsed the
//! header. That ABI was removed on 2026-09-29 -- see
//! `.agents/docs/LTM/removed-cdylib-plugin-abi.md`, which preserves the header --
//! and what survives is what the out-of-process channel still needs: a status
//! code to put in [`crate::ipc::Frame::Fault`], and a role to report in the
//! greeting.
//!
//! The numeric values are unchanged, deliberately. They travel on the wire now
//! rather than across a function table, so changing them would break a peer for
//! no reason other than tidiness.
//!
//! The module keeps its name because [`Status`] is part of the wire surface a
//! peer reads, and renaming a public module is a breaking change charged to
//! consumers in exchange for nothing.

/// Status codes. Values are part of the ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Status {
    Ok = 0,
    InvalidArgument = 1,
    Internal = 2,
    /// No database in the slot: a follower is rebootstrapping.
    Unavailable = 3,
    /// The pinned version was evicted.
    SnapshotTooOld = 4,
    /// The database was replaced; every handle is stale.
    GenerationChanged = 5,
    WrongRole = 6,
    AbiMismatch = 7,
    /// A block is open when it should not be, or not open when it should.
    BlockState = 8,
}

impl Status {
    /// A stable short name, for a plugin's logs.
    pub fn name(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::InvalidArgument => "INVALID_ARGUMENT",
            Status::Internal => "INTERNAL",
            Status::Unavailable => "UNAVAILABLE",
            Status::SnapshotTooOld => "SNAPSHOT_TOO_OLD",
            Status::GenerationChanged => "GENERATION_CHANGED",
            Status::WrongRole => "WRONG_ROLE",
            Status::AbiMismatch => "ABI_MISMATCH",
            Status::BlockState => "BLOCK_STATE",
        }
    }
}

/// Which end of a replication pair this server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Role {
    Leader = 0,
    Follower = 1,
}

impl Role {
    /// Anything that is not `Follower` reads as `Leader`.
    ///
    /// Total on purpose: this converts a value from an atomic that the server
    /// writes, and a panic in a read path the C ABI wraps would have to cross
    /// `catch_unwind` to report an impossible state.
    pub fn from_raw(v: u64) -> Role {
        if v == Role::Follower as u64 {
            Role::Follower
        } else {
            Role::Leader
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes a peer reads are frozen, and nothing else asserts that now.
    ///
    /// They used to be checked against `yesno_plugin.h` by parsing it, so the
    /// header was the second opinion. The header is gone, and these values did
    /// not stop being a contract when it went -- they travel in
    /// `Frame::Fault` -- so the second opinion is written down here instead.
    /// Renumbering is a wire break, not a refactor.
    #[test]
    fn the_status_and_role_codes_are_frozen() {
        assert_eq!(Status::Ok as u32, 0);
        assert_eq!(Status::InvalidArgument as u32, 1);
        assert_eq!(Status::Internal as u32, 2);
        assert_eq!(Status::Unavailable as u32, 3);
        assert_eq!(Status::SnapshotTooOld as u32, 4);
        assert_eq!(Status::GenerationChanged as u32, 5);
        assert_eq!(Status::WrongRole as u32, 6);
        assert_eq!(Status::AbiMismatch as u32, 7);
        assert_eq!(Status::BlockState as u32, 8);
        assert_eq!(Role::Leader as u32, 0);
        assert_eq!(Role::Follower as u32, 1);
    }
}
