//! Canonical Arrow schemas.
//!
//! Every field is `nullable = false`. See the crate docs for why a validity
//! buffer would be actively harmful here rather than merely wasteful.

use std::collections::HashMap;
use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};

/// Metadata key carrying the first ordinal a mask chunk covers.
pub const META_BASE_ORDINAL: &str = "yesno.base_ordinal";
/// Metadata key carrying the key a batch belongs to.
pub const META_KEY: &str = "yesno.key";

/// `{ ordinal: UInt64 }` — one set's ordinals.
pub fn ordinals_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![Field::new(
        "ordinal",
        DataType::UInt64,
        false,
    )]))
}

/// `{ key: UInt64, ordinal: UInt64 }`, sorted lexicographically.
///
/// The ordering is free ( the store already yields it ) and lets a query engine
/// elide a sort, so it is worth advertising rather than discarding.
pub fn pairs_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::UInt64, false),
        Field::new("ordinal", DataType::UInt64, false),
    ]))
}

/// `{ matched: Boolean }` — a selection mask over one chunk.
///
/// `base_ordinal` rides in the field metadata so a consumer can align the mask
/// against its own row numbering without a side channel.
pub fn mask_chunk_schema(base_ordinal: u64) -> SchemaRef {
    let mut md = HashMap::new();
    md.insert(META_BASE_ORDINAL.to_string(), base_ordinal.to_string());
    Arc::new(Schema::new(vec![Field::new(
        "matched",
        DataType::Boolean,
        false,
    )
    .with_metadata(md)]))
}

/// `{ key, prefix48, kind, cardinality, payload }` — containers, verbatim.
///
/// The **dump and backup** format, and the interchange one: a payload here is
/// the container's Roaring-spec bytes, byte-identical to what the page store
/// holds and to what a `.roaring` file holds. So a dump is a copy rather than a
/// re-encoding, and reading one back is `O( container count )`.
///
/// `kind` and `cardinality` are **not** derivable from `payload` alone, which
/// is why they are columns rather than a comment. An array of `n` values and a
/// run of `n / 2` intervals can be the same length in bytes; a bitmap's
/// cardinality is a popcount the reader should not have to redo; and
/// `codec::decode` takes all three because it cannot infer any of them. This
/// mirrors the on-disk `ChunkRef`, which carries `kind` and `card_m1` beside the
/// cell for exactly the same reason.
pub fn containers_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("key", DataType::UInt64, false),
        Field::new("prefix48", DataType::UInt64, false),
        Field::new("kind", DataType::UInt8, false),
        Field::new("cardinality", DataType::UInt32, false),
        Field::new("payload", DataType::Binary, false),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_nullable() {
        for s in [
            ordinals_schema(),
            pairs_schema(),
            mask_chunk_schema(0),
            containers_schema(),
        ] {
            for f in s.fields() {
                assert!(!f.is_nullable(), "field {} must not be nullable", f.name());
            }
        }
    }

    #[test]
    fn mask_schema_carries_its_base_ordinal() {
        let s = mask_chunk_schema(65_536);
        let md = s.field(0).metadata();
        assert_eq!(md.get(META_BASE_ORDINAL).map(String::as_str), Some("65536"));
    }

    #[test]
    fn pairs_are_key_then_ordinal() {
        let s = pairs_schema();
        assert_eq!(s.field(0).name(), "key");
        assert_eq!(s.field(1).name(), "ordinal");
    }
}
