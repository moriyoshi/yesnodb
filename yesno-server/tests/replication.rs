//! Replication on the shared control listener: a real follower, built from a
//! running `yesnod`.
//!
//! **This port is held to a stricter standard than Flight, and the tests are
//! shaped around why.** `FetchBaseSnapshot` streams whole shard images and
//! `FetchManifest` hands over the database's identity, so an unauthenticated
//! replication capability is a complete exfiltration primitive in one call — worse
//! than an open Flight port, which at least makes an attacker ask for keys one
//! at a time. Hence mutual TLS and a `replica` principal by default, and an
//! override that is its own flag rather than part of `--insecure`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use yesno_core::{Db, DbOptions};
use yesno_server::replication::pb::replication_client::ReplicationClient;
use yesno_server::replication::{pb, FollowerClient};

struct Pki {
    dir: PathBuf,
    ca_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
    client_fingerprint: String,
    client2_cert_pem: String,
    client2_key_pem: String,
    client2_fingerprint: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::new();
    for b in d.as_ref() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn mint(dir: &Path) -> Pki {
    use rcgen::{CertificateParams, DnType, Issuer, KeyPair};

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "yesno replication CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.clone().self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let mut srv = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    srv.distinguished_name.push(DnType::CommonName, "localhost");
    let srv_key = KeyPair::generate().unwrap();
    let srv_cert = srv.signed_by(&srv_key, &issuer).unwrap();

    let mut cli = CertificateParams::new(vec!["standby".to_string()]).unwrap();
    cli.distinguished_name.push(DnType::CommonName, "standby-b");
    let cli_key = KeyPair::generate().unwrap();
    let cli_cert = cli.signed_by(&cli_key, &issuer).unwrap();

    // A **second** replica identity. Needed because the property under test
    // is that two followers are told apart, and one certificate cannot fail
    // that test however it is used.
    let mut cli2 = CertificateParams::new(vec!["standby-c".to_string()]).unwrap();
    cli2.distinguished_name
        .push(DnType::CommonName, "standby-c");
    let cli2_key = KeyPair::generate().unwrap();
    let cli2_cert = cli2.signed_by(&cli2_key, &issuer).unwrap();

    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("server.pem"), srv_cert.pem()).unwrap();
    std::fs::write(dir.join("server.key"), srv_key.serialize_pem()).unwrap();

    Pki {
        dir: dir.to_path_buf(),
        ca_pem: ca.pem(),
        client_cert_pem: cli_cert.pem(),
        client_key_pem: cli_key.serialize_pem(),
        client_fingerprint: sha256_hex(cli_cert.der()),
        client2_cert_pem: cli2_cert.pem(),
        client2_key_pem: cli2_key.serialize_pem(),
        client2_fingerprint: sha256_hex(cli2_cert.der()),
    }
}

fn leader_config(data: &Path, pki: &Pki, shards: usize) -> yesno_server::Config {
    let mut c = yesno_server::Config::default();
    c.server.data_dir = Some(data.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.control.listen = "127.0.0.1:0".into();
    c.server.control.journal_dir = pki.dir.join("control-journal");
    c.server.control.tls.cert = Some(pki.dir.join("server.pem"));
    c.server.control.tls.key = Some(pki.dir.join("server.key"));
    c.server.control.tls.client_ca = Some(pki.dir.join("ca.pem"));
    c.server.control.tls.require_client_auth = true;
    c.server.shutdown_grace_secs = 10;
    c.db.shards = shards;
    c.auth
        .principals
        .push(yesno_server::config::PrincipalConfig {
            name: "standby-b".into(),
            role: yesno_server::config::PrincipalRole::Replica,
            token_sha256: None,
            cert_sha256: Some(pki.client_fingerprint.clone()),
        });
    c.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "standby-b".into(),
        address: "all".into(),
        capability: yesno_server::config::EndpointCapability::Replication,
        action: yesno_server::config::RuleAction::Allow,
    });
    c
}

async fn follower_channel(
    addr: std::net::SocketAddr,
    pki: &Pki,
    identity: bool,
) -> Result<tonic::transport::Channel, tonic::transport::Error> {
    let mut tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(pki.ca_pem.clone()))
        .domain_name("localhost");
    if identity {
        tls = tls.identity(tonic::transport::Identity::from_pem(
            pki.client_cert_pem.clone(),
            pki.client_key_pem.clone(),
        ));
    }
    tonic::transport::Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)?
        .connect()
        .await
}

/// The whole operator sequence: a running leader, and a follower built from
/// nothing but its socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_is_built_from_a_running_leader_over_mutual_tls() {
    const SHARDS: usize = 4;
    let pki_dir = tempfile::tempdir().unwrap();
    let leader_dir = tempfile::tempdir().unwrap();
    let follower_dir = tempfile::tempdir().unwrap();
    let pki = mint(pki_dir.path());

    let cfg = leader_config(leader_dir.path(), &pki, SHARDS);
    cfg.validate_with(false, false)
        .expect("mutual TLS plus a replica principal must be accepted");
    let running = yesno_server::start(&cfg).await.unwrap();
    let repl_addr = running.control_addr.expect("replication is enabled");

    // Write through the leader's own handle, and checkpoint so that some state
    // is in the image and some is only in the log — the follower has to get
    // both, by two different routes.
    let db = running.db();
    let keys: Vec<u64> = (0..48u64).collect();
    for k in &keys {
        db.insert_range(*k, k * 1000, k * 1000 + 30).unwrap();
    }
    db.checkpoint().unwrap();
    for k in &keys {
        db.insert(*k, k * 1000 + 500).unwrap();
    }
    let want: Vec<BTreeSet<u64>> = {
        let snap = db.snapshot().unwrap();
        keys.iter()
            .map(|k| snap.load(*k).unwrap().iter().collect())
            .collect()
    };

    // ---- the follower, over the wire only
    let ch = follower_channel(repl_addr, &pki, true)
        .await
        .expect("an authenticated follower must connect");
    let mut client = ReplicationClient::new(ch);

    let mut follower = FollowerClient::new(follower_dir.path(), 0, []);
    follower
        .fetch_manifest(&mut client)
        .await
        .expect("the leader must hand over its MANIFEST");

    let st = client
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(st.shard_count as usize, SHARDS);

    for shard in 0..SHARDS as u32 {
        let off = follower.bootstrap_shard(&mut client, shard).await.unwrap();
        follower
            .catch_up_shard(&mut client, shard, off, 128 * 1024)
            .await
            .unwrap();
        follower.ack(&mut client, shard).await.unwrap();
    }

    drop(db);
    drop(client);
    running.shutdown().await;

    // ---- set equality, which is the only comparison that means anything here
    //
    // Not byte equality: the follower runs its own allocator and checkpointer,
    // so it may hold a Bitmap where the leader holds an Array for the same
    // chunk. Comparing bytes would report divergence on a healthy pair.
    let replica = Db::open_with(
        follower_dir.path(),
        DbOptions {
            shards: 1, // wrong on purpose; the MANIFEST must win
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(replica.shard_count(), SHARDS);
    let snap = replica.snapshot().unwrap();
    for (i, k) in keys.iter().enumerate() {
        let got: BTreeSet<u64> = snap.load(*k).unwrap().iter().collect();
        assert_eq!(&got, &want[i], "key {k} diverged on the follower");
    }
}

/// A valid certificate still needs an explicit replication allow row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_deny_blocks_replication() {
    let pki_dir = tempfile::tempdir().unwrap();
    let leader_dir = tempfile::tempdir().unwrap();
    let pki = mint(pki_dir.path());

    let mut cfg = leader_config(leader_dir.path(), &pki, 1);
    cfg.auth.rules[0].action = yesno_server::config::RuleAction::Deny;
    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.control_addr.unwrap();

    let ch = follower_channel(addr, &pki, true).await.unwrap();
    let mut client = ReplicationClient::new(ch);

    let e = client.status(pb::StatusRequest {}).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied, "{e}");
    assert!(e.message().contains("denies"), "{}", e.message());

    let e = client
        .fetch_base_snapshot(pb::SnapshotRequest { shard: 0 })
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);

    let e = client
        .fetch_manifest(pb::ManifestRequest {})
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);

    running.shutdown().await;
}

/// Whole-database replication needs TLS, authentication, and an allow row.
#[test]
fn serving_replication_demands_tls_and_an_explicit_rule() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = yesno_server::Config::default();
    c.server.data_dir = Some(dir.path().to_path_buf());
    c.server.control.listen = "127.0.0.1:50052".into();
    c.server.control.journal_dir = dir.path().join("journal");

    let e = c.validate_with(false, false).unwrap_err();
    assert!(format!("{e}").contains("[[auth.rule]]"), "{e}");

    c.auth
        .principals
        .push(yesno_server::config::PrincipalConfig {
            name: "ops".into(),
            role: yesno_server::config::PrincipalRole::Admin,
            token_sha256: Some("0".repeat(64)),
            cert_sha256: None,
        });
    c.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "ops".into(),
        address: "127.0.0.0/8".into(),
        capability: yesno_server::config::EndpointCapability::Replication,
        action: yesno_server::config::RuleAction::Allow,
    });
    let e = c.validate_with(false, false).unwrap_err();
    assert!(format!("{e}").contains("without TLS"), "{e}");

    c.server.control.tls.cert = Some("/x/c.pem".into());
    c.server.control.tls.key = Some("/x/k.pem".into());
    c.validate_with(false, false)
        .expect("TLS, an authenticated principal, and an allow row must be accepted");

    let mut bare = yesno_server::Config::default();
    bare.server.data_dir = Some(dir.path().to_path_buf());
    bare.server.control.listen = "127.0.0.1:50053".into();
    bare.server.control.journal_dir = dir.path().join("bare-journal");
    bare.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "all".into(),
        address: "all".into(),
        capability: yesno_server::config::EndpointCapability::Replication,
        action: yesno_server::config::RuleAction::Allow,
    });
    assert!(
        bare.validate_with(true, false).is_err(),
        "--insecure alone opened replication"
    );
    bare.validate_with(false, true)
        .expect("--insecure-replication must be the explicit override");
}

/// Flight and the shared control plane are separate protocol boundaries.
#[test]
fn flight_and_control_cannot_share_a_port() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = yesno_server::Config::default();
    c.server.data_dir = Some(dir.path().to_path_buf());
    c.server.flight.listen = "127.0.0.1:50051".into();
    c.server.control.listen = "127.0.0.1:50051".into();
    c.server.control.journal_dir = dir.path().join("journal");
    c.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "all".into(),
        address: "all".into(),
        capability: yesno_server::config::EndpointCapability::ControlRead,
        action: yesno_server::config::RuleAction::Allow,
    });
    let e = c.validate_with(false, true).unwrap_err();
    assert!(format!("{e}").contains("both 127.0.0.1:50051"), "{e}");
}

/// **The wiring, not the mechanism.** `RetentionFloor`'s own tests prove that
/// per-follower entries behave; this proves that the identity a **certificate**
/// established actually reaches them.
///
/// That gap is the one this project keeps finding — `Follower` with no caller
/// while the M7 gate hand-rolled its own, a `db_uuid` written everywhere and
/// compared nowhere, `enforce_space_amp` reached only from a test. A unit test
/// on `observe_from` would pass with the extension never inserted, and the floor
/// would quietly collapse to the anonymous entry: the pessimistic reading, which
/// looks like working software right up until a slow standby is cut out of the
/// log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_standbys_are_told_apart_by_their_certificates() {
    let pki_dir = tempfile::tempdir().unwrap();
    let leader_dir = tempfile::tempdir().unwrap();
    let pki = mint(pki_dir.path());

    let mut cfg = leader_config(leader_dir.path(), &pki, 2);
    // Long, so the daemon's own checkpointer cannot call `take` underneath
    // this test — every `take` is a window boundary, and a second caller would
    // age the window at twice the rate.
    cfg.db.checkpoint.interval_secs = 3600;
    cfg.auth
        .principals
        .push(yesno_server::config::PrincipalConfig {
            name: "standby-c".into(),
            role: yesno_server::config::PrincipalRole::Replica,
            token_sha256: None,
            cert_sha256: Some(pki.client2_fingerprint.clone()),
        });
    cfg.auth.rules.push(yesno_server::config::AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "standby-c".into(),
        address: "all".into(),
        capability: yesno_server::config::EndpointCapability::Replication,
        action: yesno_server::config::RuleAction::Allow,
    });

    let running = yesno_server::start(&cfg).await.unwrap();
    let repl_addr = running.control_addr.expect("replication is enabled");
    let floor = running.db().retention_floor();

    async fn ack_as(
        addr: std::net::SocketAddr,
        ca: String,
        cert: String,
        key: String,
        shard: u32,
        lsn: u64,
    ) {
        let tls = tonic::transport::ClientTlsConfig::new()
            .ca_certificate(tonic::transport::Certificate::from_pem(ca))
            .domain_name("localhost")
            .identity(tonic::transport::Identity::from_pem(cert, key));
        let ch = tonic::transport::Channel::from_shared(format!("https://{addr}"))
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .expect("a replica credential must connect");
        let mut c = ReplicationClient::new(ch);
        c.ack(futures::stream::iter([pb::AckRequest {
            shard,
            applied_lsn: lsn,
        }]))
        .await
        .expect("an ack from a replica principal must be accepted");
    }

    let ack_b = |lsn| {
        ack_as(
            repl_addr,
            pki.ca_pem.clone(),
            pki.client_cert_pem.clone(),
            pki.client_key_pem.clone(),
            0,
            lsn,
        )
    };
    let ack_c = |lsn| {
        ack_as(
            repl_addr,
            pki.ca_pem.clone(),
            pki.client2_cert_pem.clone(),
            pki.client2_key_pem.clone(),
            0,
            lsn,
        )
    };

    // ---- both report, one far behind
    ack_b(100).await;
    ack_c(9_000).await;
    assert_eq!(
        floor.holders(0),
        2,
        "the two certificates were not told apart; they collapsed to one entry"
    );

    // ---- one window in which only the fast one speaks
    //
    // This is the case that used to cut `standby-b` out of the log and cost it
    // a fresh copy of the whole database.
    ack_c(12_000).await;
    assert_eq!(
        floor.take(0),
        Some(100),
        "the slow standby lost its retention floor after one silent window"
    );

    running.shutdown().await;
}
