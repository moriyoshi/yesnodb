use prost::Message;
use yesno_flight::ServerStats;

const CANONICAL: &[u8] = &[
    0x08, 0x80, 0x08, 0x10, 0x40, 0x18, 0x80, 0x01, 0x20, 0x02, 0x28, 0x04,
];

#[test]
fn server_stats_field_numbers_match_the_public_proto() {
    let stats = ServerStats::decode_protobuf(CANONICAL).unwrap();
    assert_eq!(stats.allocated_bytes, 1024);
    assert_eq!(stats.deferred_bytes, 64);
    assert_eq!(stats.wal_bytes, 128);
    assert_eq!(stats.live_readers, 2);
    assert_eq!(stats.shards, 4);
    assert_eq!(stats.encode_to_vec(), CANONICAL);
}

/// Tag 6 is `features`, and its position is part of the wire format.
///
/// Pinned here for the reason the test above pins the others: a field that
/// moves is a field every older client reads as something else. A server
/// without the capability omits it, and proto3 decodes the absence as zero,
/// which is the right answer -- it supports none of the bits.
#[test]
fn server_stats_features_ride_at_tag_six() {
    let mut with_features = CANONICAL.to_vec();
    with_features.extend_from_slice(&[0x30, 0x03]);
    let stats = ServerStats::decode_protobuf(with_features).unwrap();
    assert_eq!(stats.features, 3);
    assert_eq!(stats.shards, 4, "the earlier fields still decode");
    assert_eq!(
        ServerStats::decode_protobuf(CANONICAL).unwrap().features,
        0,
        "an encoding without the field means no capabilities"
    );
}

#[test]
fn server_stats_ignore_unknown_fields_and_reject_truncation() {
    let mut extended = CANONICAL.to_vec();
    // Tag 7, because tag 6 became `features` and is no longer unknown. The
    // point of this test is a field number this build does not know, so it has
    // to move whenever one is claimed.
    extended.extend_from_slice(&[0x38, 0x63]);
    assert_eq!(
        ServerStats::decode_protobuf(extended).unwrap(),
        ServerStats::decode_protobuf(CANONICAL).unwrap()
    );
    assert!(ServerStats::decode_protobuf([0x08, 0x80]).is_err());
}
