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

#[test]
fn server_stats_ignore_unknown_fields_and_reject_truncation() {
    let mut extended = CANONICAL.to_vec();
    extended.extend_from_slice(&[0x30, 0x63]);
    assert_eq!(
        ServerStats::decode_protobuf(extended).unwrap(),
        ServerStats::decode_protobuf(CANONICAL).unwrap()
    );
    assert!(ServerStats::decode_protobuf([0x08, 0x80]).is_err());
}
