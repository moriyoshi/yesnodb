# On-disk format

This document describes the files that make up a persistent yesnodb database,
their byte layouts, and the publication and recovery rules that give those
bytes meaning. It describes the current pre-release format. The database
directory is not an interchange format, and compatibility should be determined
from the version and feature fields below rather than inferred from a project
release number.

Container payloads deliberately match the portable Roaring representation, but
the database directory around them is yesnodb-specific. A portable Roaring file
is therefore not a shard image, and a shard image is not directly consumable as
a portable Roaring file.

## Conventions

- Byte ranges are half-open: `8..16` contains bytes 8 through 15.
- Unless a field is explicitly described as big-endian or native-endian,
  integers are unsigned and little-endian.
- Bit ranges number the least-significant bit as bit 0.
- Checksums are CRC32C ( Castagnoli ). The check value for the nine ASCII bytes
  `123456789` is `0xe3069283`.
- Reserved bytes and bits are written as zero. A reader must not assign them a
  meaning without a corresponding format or feature change.
- File offsets are absolute byte offsets from the beginning of the containing
  file. Structure-relative offsets are called out explicitly.

## Database directory

A writer-created database directory contains the following artifacts:

| name | role | recoverable database state |
|---|---|---|
| `MANIFEST` | database identity, leadership term, physical shard count, and virtual-shard routing | yes |
| `shard-NNNN.yno` | one checkpointed shard image | yes |
| `shard-NNNN.wal` | active WAL generation for one shard | yes, between checkpoints |
| `shard-NNNN.wal.BBBBBBBBBBBBBBBBBBBB` | immutable sealed WAL generation whose first LSN is `BBBBBBBBBBBBBBBBBBBB` | yes, while retained |
| `LOCK` | advisory writer lock and monotonically increasing open epoch | no |
| `READERS` | cross-process snapshot retention registry | no |

`NNNN` is a zero-padded decimal physical shard number, starting at `0000`.
`BBBBBBBBBBBBBBBBBBBB` is a 20-digit decimal global LSN. The number of shards
and the mapping from 256 virtual shards to them come from `MANIFEST`, not from
directory enumeration.

An old database may also contain a 16-byte `UUID` file. It is a migration input
used only when `MANIFEST` is absent; new directories carry the identity in the
manifest.

The recoverable state is the manifest, every shard image named by it, and any
WAL suffix not yet incorporated into those images. `LOCK` and `READERS` contain
coordination state for running processes and are reconstructed or reclaimed by
normal opens.

## `MANIFEST`

`MANIFEST` is exactly 8192 bytes: two independent 4096-byte slots, A at offset
0 and B at offset 4096. Each slot is self-checksummed. Of the slots whose magic
and checksum are valid, the slot with the greater sequence number is live. A
tie selects A. A torn slot is ignored; a checksummed slot with an unsupported
format is an incompatibility, not a torn-write fallback.

Updates publish one slot at a time. A manifest update writes only the slot that
is not currently live, in place and without truncating the file, and syncs it
before returning; the live slot keeps its bytes throughout. A slot becomes
authoritative solely by carrying the greater sequence number, and that number is
inside the checksummed range, so a partially written slot fails its checksum and
its sequence number is never observable. A crash at any byte offset of an update
therefore leaves the previous manifest readable until the new slot is complete,
and the new manifest afterwards; it never leaves the file unreadable. Every
update must raise the sequence number, or the newly written slot would not
become live. The sync before returning is ordering rather than caution: the next
update overwrites the slot this one left alone, so this slot's bytes must be
durable first, or a single crash could damage both generations.

Creation is the one write that fills both slots, and only because there is no
previous generation to preserve: a database being created serializes the same
new slot image twice, so that a torn write to one slot still leaves a complete
manifest in the other. Standby seeding writes a whole manifest by temporary file
and rename. Slot alternation matches shard superblocks, which publish the same
way.

### Manifest slot

| bytes | type | field |
|---|---|---|
| `0..8` | `u64` | magic `0x3146494e414d4e59`, stored as the ASCII bytes `YNMANIF1` |
| `8..10` | `u16` | format major; current writer emits 2 |
| `10..12` | `u16` | virtual-shard count; must be 256 |
| `12..16` | | reserved |
| `16..24` | `u64` | slot sequence number |
| `24..40` | 16 bytes | database identity |
| `40..44` | `u32` | physical shard count |
| `44..48` | `u32` | leadership term |
| `48..560` | 256 x `u16` | virtual-shard-to-physical-shard map |
| `560..564` | `u32` | CRC32C of bytes `0..560` |
| `564..4096` | | reserved |

Manifest major 1 used bytes `44..48` as zeroed slack. A current reader accepts
major 1 as leadership term 0 and writes major 2 on the next manifest update.
Every routing entry must be less than the physical shard count.

For a newly created database, virtual shard `v` maps to `v mod shard_count`.
Readers must nevertheless consult the stored map: the indirection is part of
the format and permits a future routing map to differ from the creation-time
default.

## `LOCK`

The first eight bytes are a little-endian `u64` open epoch. A writer takes an
exclusive advisory lock, reads the previous value ( or zero for a short file ),
increments it, writes it back, and synchronizes it before opening the database
for writes. The lock itself is held by the operating system; the numeric epoch
does not confer ownership.

The epoch is also written into an `EpochFence` record in each writable shard's
WAL. It distinguishes records produced by different opens of the same database.

## `READERS`

`READERS` begins with a fixed 98,304-byte array of 4096 slots at offset 0. That
prefix has no header or version field. Each 24-byte slot is:

| bytes within slot | type | field |
|---|---|---|
| `0..8` | `u64` | process ID; zero means free |
| `8..16` | `u64` | pinned visible commit version |
| `16..24` | `u64` | pinned checkpoint sequence |

A reader publishes the version and checkpoint sequence before publishing its
process ID. A writer treats a nonzero slot as live only while that process ID is
live. The file protects extent retention across processes; it contains no user
data and is not replayed during recovery.

### Reader identity region

A process ID alone cannot distinguish a reader from an unrelated process that
later received the same ID, and a recycled ID makes a departed reader's slot
look live — which retains extents that could have been freed. An identity region
follows the slot array and records, per slot, a second identifier that the
liveness check compares.

Because the slot array carries no version field, it could not be widened: a
build predating the region reads 24-byte slots from offset 0 unconditionally,
has nowhere to learn that the stride changed, and would claim a slot a live
reader holds. The region is therefore **appended**, so every byte below 98,304
keeps its meaning and an older build reads the prefix exactly as before. The
file only ever grows.

| bytes from the start of the region | type | field |
|---|---|---|
| `0..8` | `u64` | magic, the ASCII bytes `YNRIDENT` |
| `8..16` | `u64` | region format version |
| `16..24` | `u64` | entry stride in bytes |
| `24..32` | `u64` | entry count; matches the slot count |
| `32..` | | one 16-byte entry per slot: process ID, then start time |

An entry is written before the slot's process ID and cleared after it, the
reverse of the claim order, so a departing reader cannot erase an entry
belonging to a process that has already taken the slot.

Every ambiguity resolves toward *live*, because declaring a live reader dead
would reclaim extents underneath it, whereas retaining extents for a departed
one merely wastes space. A slot is treated as reclaimable only when both start
times are known and differ. A file with no region, an unrecognized magic,
version, stride or count, an entry naming a different process ID, a recorded
start time of zero, and a platform or configuration where the start time cannot
be read all fall back to process-ID-only liveness.

Start times are measured from boot, so a registry file that survives a reboot
can in principle see a coincidental match. That direction makes a departed
reader look live, which is the conservative one.

## Shard image overview

Each `shard-NNNN.yno` is a sparse, append-grown address space. It contains two
superblock slots followed by 2 MiB slabs. Slab addresses are defined by the
following geometry:

```text
slab_base(N) = N * 2 MiB
metadata     = slab_base(N) .. slab_base(N) + 8192
body         = slab_base(N) + 8192 .. slab_base(N) + 2 MiB
slot(N,i,c)  = slab_base(N) + 8192 + i * class_size[c]
```

Slab 0 is reserved. Its nominal metadata region, bytes `0..8192`, is occupied
by the two superblocks, and its body is never allocated. Allocatable extents
therefore begin in slab 1. The file never shrinks; freed slots and whole slabs
may be reused after the snapshot and A/B-superblock retention rules permit it.

```text
0                    4096                 8192
+---------------------+---------------------+
| superblock A        | superblock B        |  slab 0 metadata address
+---------------------+---------------------+
|             reserved slab 0 body                               |
+----------------------------------------------------------------+ 2 MiB
| slab 1 metadata      | slab 1 size-class slots ...             |
+----------------------------------------------------------------+ 4 MiB
| slab 2 metadata      | slab 2 size-class slots ...             |
+----------------------------------------------------------------+
```

The image is little-endian and is intentionally refused on an opposite-endian
host. The one exception is the endian probe itself, which is written in native
byte order so that an opposite-endian open detects the mismatch.

## Superblock

Superblock A occupies shard bytes `0..4096`; superblock B occupies
`4096..8192`. The readable slot with the greater sequence number names the live
checkpoint. A tie selects A. The next checkpoint overwrites the other slot.

The CRC covers the entire 4096-byte slot with bytes `4092..4096` treated as
zero. A bad magic or CRC makes that slot absent. Once the CRC is valid, an
unsupported major version, required feature, endian probe, or invalid geometry
is a hard error rather than permission to reinterpret the bytes.

| bytes | type | field |
|---|---|---|
| `0..8` | `u64` | magic `0x594e4f534e444231` |
| `8..12` | native `u32` | endian probe `0x01020304` |
| `12..14` | `u16` | format major, currently 1 |
| `14..16` | `u16` | format minor, currently 0 |
| `16..24` | `u64` | required feature bits |
| `24..32` | `u64` | compatible feature bits |
| `32..48` | 16 bytes | database identity; must match `MANIFEST` |
| `48..56` | `u64` | superblock sequence number |
| `56..60` | `u32` | physical shard number |
| `60..64` | `u32` | base page size, currently 4096; zero denotes a file written before this field was compared |
| `64..68` | `u32` | slab size, currently 2 MiB; zero denotes a file written before this field was compared |
| `68..72` | `u32` | maximum packed payload length, currently 2028 |
| `72..76` | `u32` | number of size classes, at most 11 |
| `76..120` | up to 11 x `u32` | persisted size-class ladder |
| `120..124` | `u32` | root index page ID; `0xffffffff` means no root |
| `124` | `u8` | root height; zero also means no root |
| `125..128` | | reserved |
| `128..136` | `u64` | commit version represented by the checkpoint |
| `136..144` | `u64` | LSN at which WAL replay begins |
| `144..152` | `u64` | live payload bytes reported by the checkpoint |
| `152..156` | `u32` | number of slabs known to the allocator |
| `156..160` | | reserved |
| `160..168` | `u64` | checkpoint sequence |
| `168..172` | `u32` | index-node size; zero denotes the historical 1024-byte default |
| `172..4092` | | reserved |
| `4092..4096` | `u32` | slot CRC32C |

Required feature bits change interpretation and must be understood by a reader.
Compatible feature bits may be ignored. The current format defines the
following required-bit assignments, although the current writer sets all of
them to zero:

| bit | meaning |
|---|---|
| 0 | alternative array encoding is present |
| 1 | version 2 leaf-node layout is present |
| 2 | the shard exceeds the default address-space cap |
| 3 | version 2 packed pages are present |

The size-class ladder and index-node size are stored to make shard geometry
self-describing. Every class size must be a multiple of 64, and standalone
classes must increase strictly.

Stored descriptors are enforced to different degrees, and the difference is
not cosmetic. The index-node size is honored: nodes are read at the size the
file records. The base page size, the slab size, and the size-class ladder
are compared against the ones the reader implements, and an open is refused
when they differ. That refusal is not a compatibility mechanism, it is the
absence of one: allocator capacity, slot lookup, and new allocation are
driven by the reader's own ladder, so a file written with any other one would
not be read compatibly but at the wrong offset in every extent. Refusing is
the only sound answer until one decoded geometry drives that arithmetic. The
default geometry below therefore remains the only geometry supported in
practice, but a file outside it is now rejected rather than misread. A zero
in the base page size or slab size denotes a file written before those fields
were compared, and is read at the default. The maximum packed payload length
is recorded and not compared, because it governs only what a writer chooses
to pack and a packed page describes its own contents.

Identity is two fields. The database identity is compared against `MANIFEST`
at open. The physical shard number separates shard images *within* one
database, which the database identity cannot do, since every shard of a
database carries the same one: exchanging two shard images of one database
leaves the identity check satisfied and each shard then answers its keys out
of the other's extents. Both comparisons are performed when a shard image is
opened, so that exchange is rejected rather than served.

### Current size classes

The current writer creates shards with this ladder. Class 0 is reserved for
packed pages. The remaining classes are general allocation slots; a standalone
container extent reserves the final eight bytes of its slot for a trailer,
while an index node occupies only its persisted node size.

| class | slot bytes | maximum standalone payload bytes |
|---:|---:|---:|
| 0 | 4096 | packed-page container |
| 1 | 576 | 568 |
| 2 | 704 | 696 |
| 3 | 896 | 888 |
| 4 | 1088 | 1080 |
| 5 | 1600 | 1592 |
| 6 | 2112 | 2104 |
| 7 | 3136 | 3128 |
| 8 | 4160 | 4152 |
| 9 | 6208 | 6200 |
| 10 | 8256 | 8248 |

A slab contains only one class. Its capacity is
`floor((2 MiB - 8192) / slot_bytes)`; any tail shorter than a slot is unused.
All slot starts are 64-byte aligned.

## Slab metadata

For every slab other than slab 0, the first 8192 bytes cache its class and
occupancy. The index remains authoritative: absent, torn, unknown, or
inconsistent slab metadata is treated conservatively and may be reconstructed
by walking the committed index. Until its geometry is known, an opaque slab is
never allocated into or reclaimed. A payload read from such a slab can still
proceed, but the class-dependent standalone-trailer and packed-page identity
checks are unavailable.

| bytes within metadata block | type | field |
|---|---|---|
| `0..2` | `u16` | magic `0x5953` |
| `2` | `u8` | version, currently 1 |
| `3` | `u8` | state: 0 free, 1 in use |
| `4` | `u8` | size class |
| `5..8` | | reserved |
| `8..12` | `u32` | allocation generation |
| `12..16` | `u32` | number of occupied slots |
| `16..20` | `u32` | slot capacity |
| `20..24` | `u32` | CRC32C |
| `24..32` | | reserved |
| `32..` | little-endian `u64` words | occupancy bitmap, one bit per slot |
| after bitmap | | reserved to byte 8192 |

The CRC covers all 8192 bytes with `20..24` treated as zero. The stored occupied
count must equal the population count of the bitmap.

## Chunk identity and index

An ordinal is split into a 48-bit chunk prefix and a 16-bit within-chunk value:

```text
ordinal = (prefix << 16) | low16
```

One index entry is identified by the 112-bit pair `(key, prefix)`, numerically
`(key << 48) | prefix`. It is serialized as 14 big-endian bytes so bytewise
comparison preserves numeric order. These chunk keys are the only deliberately
big-endian integers in the image.

The index is a copy-on-write B+tree. Superblocks and internal nodes store a
32-bit page ID rather than a byte offset:

```text
node_byte_offset = page_id * 64
```

The current node size is 1024 bytes. Each node is stored at the start of a
size-class slot, is immutable after publication, and carries a CRC over the
whole node with its CRC field treated as zero.

### Common node header

| bytes within node | type | field |
|---|---|---|
| `0` | `u8` | node type: 1 leaf, 2 internal |
| `1` | `u8` | node version, currently 1 |
| `2..4` | `u16` | leaf key count or internal separator count |
| `4` | `u8` | leaf key-suffix width; unused in internal nodes |
| `5..8` | | reserved |
| `8..12` | `u32` | CRC32C |
| `12..26` | 14 bytes | leaf's first full chunk key; unused in internal nodes |
| `26..32` | | reserved |

### Leaf body

A leaf chooses a suffix width from `2, 4, 6, 8, 10, 12, 14`. Width 14 is the
uncompressed representation. The key at entry `i` is reconstructed by taking
the common high bytes from the full key in `12..26` and replacing its last
`suffix_width` bytes with entry `i`'s suffix.

For `n` keys and suffix width `s`, the body is:

```text
32 .. 32 + n*s          n consecutive big-endian key suffixes
32 + n*s .. 32+n*s+n*8  n consecutive little-endian ChunkRef words
remainder of node        zero-filled
```

Keys are strictly ascending. Values are parallel to the suffix array: the
`i`th 64-bit word describes the `i`th reconstructed key.

### Internal-node body

If the header records `n` separators, the node has `n + 1` children:

```text
32 .. 32 + n*14             n full 14-byte big-endian separators
32 + n*14 .. 32+n*14+(n+1)*4  n+1 little-endian page IDs
remainder of node             zero-filled
```

Separator `i` is the smallest key reachable through child `i + 1`. The smallest
key of child 0 is implied by the parent and is not stored.

## `ChunkRef`

Every leaf value is one 64-bit `ChunkRef`. It either points to an out-of-line
payload or stores one to three array values inline.

### Out-of-line form

```text
bits  0..40   payload byte offset in the shard image
bits 40..56   cardinality minus one
bits 56..58   kind: 0 array, 1 bitmap, 2 run, 3 reserved
bit      58   0, selecting the out-of-line form
bit      59   encoding: 0 raw Roaring payload, 1 reserved alternative
bits 60..64   reserved
```

The 40-bit cell field addresses up to one TiB. A cardinality-minus-one value of
`0xffff` represents a full 65,536-value chunk.

### Inline form

```text
bits  0..16   first array value
bits 16..32   second array value, or zero
bits 32..48   third array value, or zero
bits 48..56   reserved
bits 56..58   kind 0 ( array )
bit      58   1, selecting the inline form
bits 59..61   value count minus one, in the range 0..2
bits 61..64   reserved
```

Inline values are little-endian `u16`, strictly ascending, and unique. Inline
chunks allocate no extent.

## Container payloads

Out-of-line payload bytes match portable Roaring container payloads:

| kind | bytes | invariants |
|---|---|---|
| array | `2 * cardinality` | ascending, unique little-endian `u16` values |
| bitmap | 8192 | 1024 little-endian `u64` words; bit `i` represents value `i` |
| run | `2 + 4 * run_count` | `u16` run count, then little-endian `(start, length_minus_one)` pairs |

Runs ascend, do not overlap, and stay within `0..65536`; the current writer
also coalesces adjacent runs into maximal intervals. The cardinality in the
index is authoritative for array and bitmap payload sizing; a run's leading
count is required to discover its byte length.

Payloads of one to three values are inline. Non-bitmap payloads of at most 2028
bytes are normally placed in packed pages. Larger payloads and every bitmap use
a standalone extent.

### Standalone extent

A standalone extent starts at the cell stored in `ChunkRef`:

```text
cell                         payload bytes
cell + payload_length       unused slot slack
cell + class_size - 8       u32 chunk-key tag
cell + class_size - 4       u32 CRC32C of payload only
```

There is no extent header and no liveness field. The index is the sole authority
on which extents are live. The fixed-position trailer detects a reference that
lands in another chunk's slot and permits offline payload verification.

The key tag is the high 32 bits of a SplitMix64-style finalizer. All operations
below wrap modulo `2^64`:

```text
z = (key XOR rotate_left(prefix, 32)) + 0x9e3779b97f4a7c15
z = (z XOR (z >> 30)) * 0xbf58476d1ce4e5b9
z = (z XOR (z >> 27)) * 0x94d049bb133111eb
tag = (z XOR (z >> 31)) >> 32
```

### Packed page

A packed page occupies one 4096-byte class-0 slot and contains several small
array or run payloads. It has no per-payload directory. Each `ChunkRef` points
directly to its payload, and the index supplies its kind, cardinality, and
length. Payloads are appended in ascending chunk-key order and are naturally
two-byte aligned because every permitted payload length is even.

| bytes within page | type | field |
|---|---|---|
| `0..2` | `u16` | magic `0x4e59`, stored as the ASCII bytes `YN` |
| `2` | `u8` | version, currently 1 |
| `3` | `u8` | required flags, currently zero |
| `4..8` | `u32` | CRC32C |
| `8..22` | 14 bytes | first chunk key, big-endian |
| `22..36` | 14 bytes | last chunk key, big-endian |
| `36..40` | | reserved |
| `40..4096` | | concatenated payloads followed by zero-filled space |

The CRC covers all 4096 bytes with `4..8` treated as zero. The first/last range
is an inexpensive identity check for a `ChunkRef`; the index remains the only
directory that identifies the exact payload position.

## WAL

Each shard WAL is an ordered sequence of immutable sealed generations followed
by one append-only active generation. A checkpoint seals the active file by
renaming it with its 20-digit starting LSN, durably publishes that directory
entry, and creates a new active file at the previous generation's end. A reopen
repairs the crash window in which the rename is durable but the empty active
file does not yet exist. Retention reclaims whole sealed files; it never moves a
surviving byte within a file.

Every generation is a sequence of independently checksummed, 8-byte-aligned
frames with no file header. The first valid frame's LSN is the global LSN of
file byte 0 and must agree with a sealed generation's filename. An empty active
generation starts at the end of the newest sealed generation, or at the
corresponding superblock's WAL replay LSN when no sealed generation remains.

An LSN is a byte position in the shard's untruncated WAL history, not a record
counter and not necessarily the current file offset:

```text
record_lsn = generation_base_lsn + generation_file_offset
```

### Record frame

| bytes within frame | type | field |
|---|---|---|
| `0..4` | `u32` | total frame length, including zero padding |
| `4..8` | `u32` | CRC32C of bytes `8..total_length` |
| `8` | `u8` | record type |
| `9` | `u8` | flags; bit `0x01` means the body carries a commit time |
| `10..12` | | reserved |
| `12..16` | `u32` | body length, excluding padding |
| `16..24` | `u64` | record LSN |
| `24..32` | `u64` | commit version |
| `32..40` | `u64` | leadership term |
| `40..40+body_length` | | record body |
| to `total_length` | | zero padding to an 8-byte boundary |

`total_length` is at least 40 and divisible by 8. The CRC protects the record
type, flags, reserved bytes, body length, LSN, commit version, term, body, and
padding. The total-length and CRC fields themselves are outside its coverage.

A sequential scan stops at the first zero header, truncated frame, CRC failure,
or LSN that disagrees with the expected position. This defines a crash-torn tail
as absent. An unknown record type under a valid checksum is instead an
unsupported format and must not be silently treated as end-of-log. A flag bit
outside the set defined here is treated the same way, for the same reason: the
checksum held, so a later writer meant those bytes, and skipping an unknown flag
would mean reading its body as something else.

### Record types and current bodies

| type | name | current body interpretation |
|---:|---|---|
| 0 | Pad | no semantic body; ignored during replay |
| 1 | ChunkDelta | key, chunk prefix, add/remove counts, then within-chunk values |
| 2 | ChunkImage | key followed by complete 64-bit ordinals |
| 3 | ChunkDelete | key |
| 4 | SetRange | key, inclusive low/high ordinals, remove flag |
| 5 | CommitIntent | physical shard IDs participating in a multi-shard commit |
| 6 | ShardCommit | empty, or an 8-byte commit time when flag `0x01` is set |
| 7 | Abort | empty, or an 8-byte commit time when flag `0x01` is set |
| 8 | CheckpointBegin | marker body ignored during replay |
| 9 | CheckpointEnd | marker body ignored during replay |
| 10 | EpochFence | open epoch |

The bodies are:

```text
ChunkDelta:
   0..8    u64 key
   8..16   u64 chunk prefix
  16..18   u16 add_count
  18..20   u16 remove_count
  20..24   reserved
  24..     add_count u16 additions, then remove_count u16 removals

ChunkImage:
   0..8    u64 key
   8..     zero or more complete u64 ordinals

ChunkDelete:
   0..8    u64 key

SetRange:
   0..8    u64 key
   8..16   u64 inclusive low ordinal
  16..24   u64 inclusive high ordinal
  24       u8 remove ( 0 insert, nonzero remove )

CommitIntent:
   0..2    u16 shard_count
   2..     shard_count little-endian u32 physical shard IDs

ShardCommit and Abort, when flag 0x01 is set:
   0..8    u64 commit time, UNIX epoch microseconds

EpochFence:
   0..8    u64 open epoch
```

For a multi-shard transaction, every participating log receives the same
commit version, a `CommitIntent` naming all participants, its local redo
records, and a `ShardCommit`. Recovery publishes only a consecutive prefix of
fully resolved commit versions. An `Abort` resolves a version that will never
complete. Redo records at or below the checkpoint version are already present
in the shard image and are not applied again.

## Publication and recovery

Container extents and B+tree nodes are immutable after publication. A
checkpoint writes replacements into unreachable slots and builds a new tree
bottom-up. Publication proceeds in this order:

1. Write new payloads and index nodes.
2. Synchronize the shard image.
3. Write the derivable slab-occupancy blocks in place and synchronize again.
4. Write the new superblock into the older A/B slot and synchronize it.

Step 4 is the checkpoint commit point. A crash before it leaves new extents
unreachable. A torn superblock write loses only the new slot, so the previous
slot and root remain usable. Slab metadata is allowed to tear because it can be
derived from the committed index.

At open, the live superblock supplies the checkpoint root, checkpoint version,
and WAL replay position. Recovery scans the valid WAL prefix, determines the
highest consecutive commit version resolved across its participating shards,
replays redo above the checkpoint version through that watermark, and truncates
an incomplete or invalid suffix. The result is a single visible commit-version
prefix; recovery does not expose a transaction on only some of its shards.

Two readable superblocks can name two generations of the tree. An extent made
obsolete by one checkpoint is therefore not immediately reusable: it must also
be unreachable from retained snapshots and from the older superblock generation,
and no zero-copy buffer may still alias it. These retention conditions are why
free space cannot be inferred by scanning for locally well-formed extent bytes.

## Integrity and compatibility summary

| structure | integrity mechanism | failure behavior |
|---|---|---|
| manifest slot | magic and CRC32C | ignore torn slot; select other slot |
| superblock slot | magic and CRC32C | ignore torn slot; select other slot |
| slab metadata | magic, version, CRC32C, count/bitmap agreement | treat as unknown and derive from index |
| index node | version, shape, and stored CRC32C | shape and version checked online; the integrity scan recomputes the stored CRC |
| standalone payload | chunk-key tag and stored payload CRC32C | tag checked online when slab geometry is known; the integrity scan recomputes the stored CRC |
| packed page | magic, version, flags, key range, and stored CRC32C | header and range checked online when class 0 is known; the integrity scan recomputes the stored CRC |
| WAL frame | length, LSN redundancy, type, CRC32C | ignore a crash-torn tail and remove its recovery suffix; reject valid unknown type |

The distinction between "stored" and "checked" in this table is deliberate. The
online read path avoids hashing up to 8 KiB on every container read: what it
verifies is *identity* — a chunk-key tag, a packed page's key range, a node's
version and shape — which costs four bytes and catches a reference that has come
to point at the wrong bytes, but says nothing about whether those bytes are
intact. Content is verified by the integrity scan instead, which recomputes
every stored index-node, packed-page and standalone-payload CRC32C and reports
each mismatch. A packed page is verified once per page rather than once per
chunk in it, so the scan's cost tracks bytes rather than chunk count. The
once-per-faulted-page checksum cache that would let the online path verify
content as well is still not implemented, so between scans a corrupt payload is
detected only when it also breaks a container invariant.

Where a checksum is enforced, a version incompatibility behind a valid checksum
is deliberately different from a torn write. What a checksum failure costs
depends on whether the region is derivable. Slab metadata is a cache of state
the committed index can reproduce, so a failure there falls back to deriving it
and loses nothing. An index node, a packed page and a standalone payload are not
derivable — the first is the authority on liveness, the other two hold the data
itself — so a failure there is reported as data loss, and a liveness result
derived from a node that failed its checksum is not adopted. Independently, a
reader that encounters an unknown major version, required feature, node version,
packed-page flag, record type, or reserved `ChunkRef` encoding must refuse it:
guessing could produce a plausible but incorrect set.
