//! The persistent chunk index: a copy-on-write B+tree keyed by [`ChunkKey`].
//!
//! Chosen over an LSM because the hot path is an **ordered range scan over one
//! key** — `[key << 48, (key+1) << 48)` — which a B+tree serves at zero read
//! amplification, while an LSM would merge across levels on every scan. Copy-on
//! write makes an MVCC snapshot a single root offset.
//!
//! COW's weakness, write amplification on scattered updates, is neutralized by a
//! memory delta layer: reads merge an in-memory map with a cursor at the
//! snapshot root, and the tree is rewritten only at checkpoint as one bottom-up
//! merge. That is "LSM with exactly one in-memory level and one on-disk level" —
//! LSM's write batching without LSM's scan read-amplification.

pub mod node;
pub mod tree;
