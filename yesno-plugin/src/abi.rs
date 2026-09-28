//! The plain-data half of the ABI: codes, kinds, and the table headers.
//!
//! Every type here has a fixed C representation and is the Rust mirror of a
//! declaration in `include/yesno_plugin.h`. A test at the bottom asserts the two
//! agree on the things a mismatch would corrupt silently -- discriminants and
//! struct size -- because a header and a `#[repr(C)]` struct are two
//! implementations of one layout and nothing but a check keeps them equal.

/// ABI version this build speaks.
pub const ABI_V1: u32 = 1;

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

    /// Map a core error onto a code a plugin can branch on.
    ///
    /// `SnapshotTooOld` is the one that must not be flattened: it means the
    /// lease is dead and a new one will work, which is completely different
    /// advice from `Internal`.
    pub fn from_core(e: &yesno_core::CodecError) -> Status {
        match e {
            yesno_core::CodecError::SnapshotTooOld { .. } => Status::SnapshotTooOld,
            _ => Status::Internal,
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

/// How a chunk's payload is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum ChunkKind {
    Array = 0,
    Bitmap = 1,
    Run = 2,
    /// This lane has nothing at this block. Reported, not omitted.
    Absent = 3,
}

/// Prefix of both tables. Never reordered.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct AbiHeader {
    pub version: u32,
    pub size: u32,
}

/// A borrowed description of one lane's chunk in the open block.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Chunk {
    pub prefix: u64,
    pub kind: u32,
    /// Values, words, or intervals, per kind. Zero when absent.
    pub count: u32,
    /// Borrowed until the block is released. Null when absent, and null from
    /// `block_lane` when the payload cannot be lent.
    pub data: *const std::ffi::c_void,
}

impl Chunk {
    pub(crate) fn absent(prefix: u64) -> Chunk {
        Chunk {
            prefix,
            kind: ChunkKind::Absent as u32,
            count: 0,
            data: std::ptr::null(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header and this module are two implementations of one layout.
    ///
    /// Parsed rather than eyeballed, because a discriminant that drifts produces
    /// a plugin that misreads every status and a container kind that decodes as
    /// the wrong representation -- both silent. Only the enumerators this crate
    /// defines are checked; the header may carry more.
    #[test]
    fn the_c_header_agrees_with_these_discriminants() {
        let header = include_str!("../include/yesno_plugin.h");
        let mut want: Vec<(&str, u32)> = vec![
            ("YESNO_OK", Status::Ok as u32),
            ("YESNO_INVALID_ARGUMENT", Status::InvalidArgument as u32),
            ("YESNO_INTERNAL", Status::Internal as u32),
            ("YESNO_UNAVAILABLE", Status::Unavailable as u32),
            ("YESNO_SNAPSHOT_TOO_OLD", Status::SnapshotTooOld as u32),
            ("YESNO_GENERATION_CHANGED", Status::GenerationChanged as u32),
            ("YESNO_WRONG_ROLE", Status::WrongRole as u32),
            ("YESNO_ABI_MISMATCH", Status::AbiMismatch as u32),
            ("YESNO_BLOCK_STATE", Status::BlockState as u32),
            ("YESNO_ROLE_LEADER", Role::Leader as u32),
            ("YESNO_ROLE_FOLLOWER", Role::Follower as u32),
            ("YESNO_CHUNK_ARRAY", ChunkKind::Array as u32),
            ("YESNO_CHUNK_BITMAP", ChunkKind::Bitmap as u32),
            ("YESNO_CHUNK_RUN", ChunkKind::Run as u32),
            ("YESNO_CHUNK_ABSENT", ChunkKind::Absent as u32),
        ];
        want.sort();
        for (name, value) in want {
            let needle = format!("{name} = ");
            let at = header
                .find(&needle)
                .unwrap_or_else(|| panic!("{name} is not declared in yesno_plugin.h"));
            let rest = &header[at + needle.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            assert_eq!(
                digits.parse::<u32>().ok(),
                Some(value),
                "{name} disagrees between the header and abi.rs"
            );
        }
    }

    /// The version the header publishes is the version this build speaks.
    #[test]
    fn the_abi_version_agrees() {
        let header = include_str!("../include/yesno_plugin.h");
        assert!(
            header.contains(&format!("#define YESNO_PLUGIN_ABI_V1 {ABI_V1}u")),
            "the header's YESNO_PLUGIN_ABI_V1 is not {ABI_V1}"
        );
    }

    /// `Chunk` must match `yesno_chunk`'s C layout: 8 + 4 + 4 + pointer.
    #[test]
    fn the_chunk_struct_has_the_layout_the_header_declares() {
        assert_eq!(
            std::mem::size_of::<Chunk>(),
            16 + std::mem::size_of::<*const std::ffi::c_void>()
        );
        assert_eq!(std::mem::align_of::<Chunk>(), 8);
        assert_eq!(std::mem::size_of::<AbiHeader>(), 8);
    }
}
