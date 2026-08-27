//! Certificate rotation without a restart, and the rotation that must be refused.
//!
//! **Certificates are minted here, at test time, and not checked in** — the
//! same reason as in `tls.rs`: a checked-in PEM expires and then the suite goes
//! red on a calendar date with a failure that reads like a code defect.
//!
//! # Why this layer
//!
//! These run a real `yesnod` listener on a real port and drive it with a real
//! Rustls client, so the handshake, the ALPN negotiation, the client-auth
//! verifier and the peer-certificate plumbing are all the production ones. What
//! a two-process drill would add over this is a process boundary and
//! replication, and certificate rotation interacts with neither. The one part
//! that *is* genuinely process-level — the signal — is covered here too, by
//! raising `SIGHUP` at this test binary and watching the material change.
//!
//! **The tests are serialized.** A reload is a process-wide event by
//! construction ( one `SIGHUP`, every listener ), so two of these running
//! concurrently would reload each other's listeners. `ROTATION` makes the
//! ordering explicit rather than leaving it to `--test-threads`.

use std::path::{Path, PathBuf};

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::FlightDescriptor;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use yesno_server::config::{Anonymous, Config, PrincipalConfig, PrincipalRole};

/// One reload at a time. See the module header.
static ROTATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A self-contained authority: its CA, a `localhost` server identity, and a
/// client identity it signed.
struct Pki {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
    /// SHA-256 of the client certificate's DER leaf — what the config matches.
    client_fingerprint: String,
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

/// Mint an independent authority. Each call produces a CA that no previous
/// call's CA vouches for, which is exactly what makes "the old certificate is
/// gone" observable: a client pinned to the old CA must stop being served.
fn mint() -> Pki {
    use rcgen::{CertificateParams, DnType, Issuer, KeyPair};

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "yesno rotation test CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.clone().self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    // The SAN must be the name the client verifies, which is `localhost`
    // below rather than `127.0.0.1`.
    let mut srv_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    srv_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    let srv_key = KeyPair::generate().unwrap();
    let srv = srv_params.signed_by(&srv_key, &issuer).unwrap();

    let mut cli_params = CertificateParams::new(vec!["client".to_string()]).unwrap();
    cli_params
        .distinguished_name
        .push(DnType::CommonName, "standby-b");
    let cli_key = KeyPair::generate().unwrap();
    let cli = cli_params.signed_by(&cli_key, &issuer).unwrap();

    Pki {
        ca_pem: ca.pem(),
        server_cert_pem: srv.pem(),
        server_key_pem: srv_key.serialize_pem(),
        client_cert_pem: cli.pem(),
        client_key_pem: cli_key.serialize_pem(),
        client_fingerprint: sha256_hex(cli.der()),
    }
}

/// The paths the server is configured with. Rotation overwrites the *contents*
/// of these files, which is what an operator's rotation actually does.
struct Live {
    cert: PathBuf,
    key: PathBuf,
    client_ca: PathBuf,
}

impl Live {
    fn at(dir: &Path) -> Self {
        Self {
            cert: dir.join("server.pem"),
            key: dir.join("server.key"),
            client_ca: dir.join("clients-ca.pem"),
        }
    }

    fn install(&self, pki: &Pki) {
        std::fs::write(&self.cert, &pki.server_cert_pem).unwrap();
        std::fs::write(&self.key, &pki.server_key_pem).unwrap();
        std::fs::write(&self.client_ca, &pki.ca_pem).unwrap();
    }
}

fn base_config(data: &Path, live: &Live) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(data.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 1;
    c.server.flight.tls.cert = Some(live.cert.clone());
    c.server.flight.tls.key = Some(live.key.clone());
    c
}

async fn client(
    addr: std::net::SocketAddr,
    tls: ClientTlsConfig,
) -> Result<FlightServiceClient<Channel>, tonic::transport::Error> {
    let ch = Channel::from_shared(format!("https://{addr}"))
        .unwrap()
        .tls_config(tls)?
        .connect()
        .await?;
    Ok(FlightServiceClient::new(ch))
}

fn descriptor(key: u64) -> FlightDescriptor {
    FlightDescriptor::new_cmd(key.to_le_bytes().to_vec())
}

fn trusting(ca_pem: &str) -> ClientTlsConfig {
    ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(ca_pem))
        .domain_name("localhost")
}

/// A fresh connection that trusts `ca_pem` is served.
async fn assert_served(addr: std::net::SocketAddr, ca_pem: &str, why: &str) {
    let mut c = client(addr, trusting(ca_pem))
        .await
        .unwrap_or_else(|e| panic!("{why}: connect failed: {e}"));
    let info = c
        .get_flight_info(descriptor(1))
        .await
        .unwrap_or_else(|e| panic!("{why}: call failed: {e}"))
        .into_inner();
    assert_eq!(info.total_records, 0, "{why}");
}

/// Who the server says the caller is.
///
/// `handshake` and not `get_flight_info`, because reaching it at all proves
/// the certificate resolved to a *principal*: the interceptor refuses anything
/// it cannot name. Asserting only that a connection was possible would pass on a
/// server that had quietly stopped asking for client certificates, which is
/// precisely the regression this file exists to catch.
async fn whoami(c: &mut FlightServiceClient<Channel>) -> Result<String, tonic::Status> {
    use futures::StreamExt as _;
    let mut hs = c
        .handshake(futures::stream::iter(
            Vec::<arrow_flight::HandshakeRequest>::new(),
        ))
        .await?
        .into_inner();
    let first = hs
        .next()
        .await
        .ok_or_else(|| tonic::Status::internal("the handshake produced no response"))??;
    Ok(String::from_utf8_lossy(&first.payload).into_owned())
}

/// A fresh connection that trusts `ca_pem` is refused.
///
/// Some stacks fail in `connect`, others defer the handshake to the first call —
/// both are a refusal, and this must accept either.
async fn assert_refused(addr: std::net::SocketAddr, ca_pem: &str, why: &str) {
    if let Ok(mut c) = client(addr, trusting(ca_pem)).await {
        assert!(
            c.get_flight_info(descriptor(1)).await.is_err(),
            "{why}: the server was still reachable"
        );
    }
}

/// The rotation that must work: new files, one trigger, new material.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_rotation_is_picked_up_and_the_old_certificate_stops_being_served() {
    let _serialized = ROTATION.lock().await;
    let tls_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let live = Live::at(tls_dir.path());

    let old = mint();
    live.install(&old);

    let mut cfg = base_config(data.path(), &live);
    cfg.auth.anonymous = Anonymous::Read;
    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;

    assert_served(addr, &old.ca_pem, "before rotation, the old CA").await;

    // Hold this one open across the rotation. A swap replaces the pointer the
    // *next* accept reads; it must not disturb a connection already negotiated.
    let mut established = client(addr, trusting(&old.ca_pem)).await.unwrap();

    // The rotation itself: overwrite the files in place, then trigger.
    let new = mint();
    live.install(&new);
    let report = yesno_server::tls::reload_all();
    assert_eq!(
        report,
        yesno_server::tls::ReloadReport {
            reloaded: 1,
            failed: 0
        },
        "the rotation was not accepted"
    );

    assert_served(addr, &new.ca_pem, "after rotation, the new CA").await;
    assert_refused(addr, &old.ca_pem, "after rotation, the old CA").await;

    // The already-negotiated connection still works. A rotation is not a
    // disconnect, and an operator rotating at noon must not drop live traffic.
    let info = established
        .get_flight_info(descriptor(1))
        .await
        .expect("an established connection was broken by a rotation")
        .into_inner();
    assert_eq!(info.total_records, 0);

    drop(established);
    running.shutdown().await;
}

/// The one that matters: a rotation that is wrong must be a logged refusal, not
/// an outage.
///
/// Three separate wrongs, because they fail at three different places and a
/// swap placed too early would pass some of them. A missing file fails at the
/// read; a truncated PEM fails at the parse; a mismatched key parses perfectly
/// and fails only when Rustls checks that it belongs to the certificate. The
/// last one is the realistic operator mistake — two rotations racing, or a
/// half-finished `scp` — and it is the one a naive "read the cert, then read the
/// key, then install" would serve to clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_rotation_is_refused_and_the_old_material_keeps_serving() {
    let _serialized = ROTATION.lock().await;
    let tls_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let live = Live::at(tls_dir.path());

    let good = mint();
    live.install(&good);

    let mut cfg = base_config(data.path(), &live);
    cfg.auth.anonymous = Anonymous::Read;
    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;
    assert_served(addr, &good.ca_pem, "before any bad rotation").await;

    let failed = yesno_server::tls::ReloadReport {
        reloaded: 0,
        failed: 1,
    };

    // 1. The certificate is gone.
    std::fs::remove_file(&live.cert).unwrap();
    assert_eq!(
        yesno_server::tls::reload_all(),
        failed,
        "a missing certificate was accepted"
    );
    assert_served(addr, &good.ca_pem, "after a missing certificate").await;

    // 2. The certificate is there and is not a certificate.
    std::fs::write(&live.cert, b"-----BEGIN CERTIFICATE-----\nnot base64\n").unwrap();
    assert_eq!(
        yesno_server::tls::reload_all(),
        failed,
        "a malformed certificate was accepted"
    );
    assert_served(addr, &good.ca_pem, "after a malformed certificate").await;

    // 3. Both files are well-formed, and the key is not this certificate's key.
    let other = mint();
    std::fs::write(&live.cert, &other.server_cert_pem).unwrap();
    std::fs::write(&live.key, &good.server_key_pem).unwrap();
    assert_eq!(
        yesno_server::tls::reload_all(),
        failed,
        "a key that does not match the certificate was accepted"
    );
    assert_served(addr, &good.ca_pem, "after a mismatched key").await;
    assert_refused(
        addr,
        &other.ca_pem,
        "the refused certificate must not be live",
    )
    .await;

    // And a *repaired* rotation still lands, so the refusals above left nothing
    // wedged.
    live.install(&other);
    assert_eq!(
        yesno_server::tls::reload_all(),
        yesno_server::tls::ReloadReport {
            reloaded: 1,
            failed: 0
        },
    );
    assert_served(addr, &other.ca_pem, "after the repair").await;

    running.shutdown().await;
}

/// The client-auth trust roots rotate too, and that is the reason the whole
/// `ServerConfig` is swapped rather than only the certificate.
///
/// Both client fingerprints are principals from the start, so nothing in the
/// authorization layer changes across the rotation. The only thing that decides
/// who is served is which CA the listener currently trusts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_the_client_ca_changes_who_is_admitted() {
    let _serialized = ROTATION.lock().await;
    let tls_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let live = Live::at(tls_dir.path());

    let old = mint();
    let new = mint();
    live.install(&old);

    let mut cfg = base_config(data.path(), &live);
    cfg.server.flight.tls.client_ca = Some(live.client_ca.clone());
    cfg.server.flight.tls.require_client_auth = true;
    for (name, pki) in [("standby-b-old", &old), ("standby-b-new", &new)] {
        cfg.auth.principals.push(PrincipalConfig {
            name: name.into(),
            role: PrincipalRole::Writer,
            token_sha256: None,
            cert_sha256: Some(pki.client_fingerprint.clone()),
        });
    }
    cfg.validate(false).unwrap();

    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;

    let identified = |server_ca: &str, cert: &str, key: &str| {
        trusting(server_ca).identity(Identity::from_pem(cert, key))
    };

    // The old client is admitted **and named**; the new one is not, because the
    // CA that signed it is not trusted yet.
    let mut c = client(
        addr,
        identified(&old.ca_pem, &old.client_cert_pem, &old.client_key_pem),
    )
    .await
    .expect("the old client certificate must be accepted before rotation");
    let who = whoami(&mut c)
        .await
        .expect("the old client must be identified before rotation");
    assert!(who.contains("standby-b-old"), "{who}");
    drop(c);

    if let Ok(mut c) = client(
        addr,
        identified(&old.ca_pem, &new.client_cert_pem, &new.client_key_pem),
    )
    .await
    {
        assert!(
            whoami(&mut c).await.is_err(),
            "a client signed by an untrusted CA was named"
        );
    }

    live.install(&new);
    assert_eq!(
        yesno_server::tls::reload_all(),
        yesno_server::tls::ReloadReport {
            reloaded: 1,
            failed: 0
        },
    );

    // Exactly the reverse, with no configuration change and no restart.
    let mut c = client(
        addr,
        identified(&new.ca_pem, &new.client_cert_pem, &new.client_key_pem),
    )
    .await
    .expect("the new client certificate must be accepted after rotation");
    let who = whoami(&mut c)
        .await
        .expect("the new client must be identified after rotation");
    assert!(who.contains("standby-b-new"), "{who}");
    drop(c);

    if let Ok(mut c) = client(
        addr,
        identified(&new.ca_pem, &old.client_cert_pem, &old.client_key_pem),
    )
    .await
    {
        assert!(
            whoami(&mut c).await.is_err(),
            "a client signed by the retired CA was still named"
        );
    }

    running.shutdown().await;
}

/// The trigger an operator will actually use.
///
/// The handler is installed before the signal is raised, and that ordering is
/// the test's whole risk: SIGHUP's default disposition is to terminate, so a
/// `raise` that beat the registration would kill this test binary rather than
/// fail an assertion. `watch_for_reload_signal` registers synchronously and
/// returns, which is what makes the ordering something the caller can rely on.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sighup_rotates_the_certificate() {
    let _serialized = ROTATION.lock().await;
    let tls_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let live = Live::at(tls_dir.path());

    let old = mint();
    live.install(&old);

    let mut cfg = base_config(data.path(), &live);
    cfg.auth.anonymous = Anonymous::Read;
    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;
    assert_served(addr, &old.ca_pem, "before SIGHUP").await;

    yesno_server::tls::watch_for_reload_signal();

    let new = mint();
    live.install(&new);
    // SAFETY: `raise` on the current process, after the handler above is
    // installed. Nothing else in this binary raises a signal.
    assert_eq!(unsafe { libc::raise(libc::SIGHUP) }, 0);

    // The handler is a task, so the swap is not synchronous with `raise`. Poll
    // for the observable effect rather than sleeping a guessed interval.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if client(addr, trusting(&new.ca_pem)).await.is_ok() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "SIGHUP did not rotate the certificate"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_refused(addr, &old.ca_pem, "after SIGHUP, the old CA").await;

    running.shutdown().await;
}
