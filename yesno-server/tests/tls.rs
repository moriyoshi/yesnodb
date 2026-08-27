//! TLS, including the refusals.
//!
//! **Certificates are minted here, at test time, and not checked in.** A
//! checked-in PEM expires, and then this suite goes red on a calendar date with
//! a failure that reads like a code defect and sends somebody hunting through
//! the handshake. `rcgen` is a dev-dependency, so it cannot reach `lean-core`.

use std::path::{Path, PathBuf};

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::FlightDescriptor;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};
use yesno_server::config::Config;

/// A CA, a server certificate for `localhost`, and a client certificate.
struct Pki {
    dir: PathBuf,
    ca_pem: String,
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

fn mint(dir: &Path) -> Pki {
    use rcgen::{CertificateParams, DnType, Issuer, KeyPair};

    // A CA.
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "yesno test CA");
    let ca_key = KeyPair::generate().unwrap();
    let ca = ca_params.clone().self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    // A server certificate. The SAN must be the name the client verifies,
    // which is `localhost` below rather than `127.0.0.1` — an IP in the URL and
    // a DNS name in the certificate is the normal case, and `domain_name` is how
    // a client says so.
    let mut srv_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    srv_params
        .distinguished_name
        .push(DnType::CommonName, "localhost");
    let srv_key = KeyPair::generate().unwrap();
    let srv = srv_params.signed_by(&srv_key, &issuer).unwrap();

    // A client certificate, signed by the same CA.
    let mut cli_params = CertificateParams::new(vec!["client".to_string()]).unwrap();
    cli_params
        .distinguished_name
        .push(DnType::CommonName, "standby-b");
    let cli_key = KeyPair::generate().unwrap();
    let cli = cli_params.signed_by(&cli_key, &issuer).unwrap();

    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    std::fs::write(dir.join("server.pem"), srv.pem()).unwrap();
    std::fs::write(dir.join("server.key"), srv_key.serialize_pem()).unwrap();

    Pki {
        dir: dir.to_path_buf(),
        ca_pem: ca.pem(),
        client_cert_pem: cli.pem(),
        client_key_pem: cli_key.serialize_pem(),
        client_fingerprint: sha256_hex(cli.der()),
    }
}

fn base_config(data: &Path, pki: &Pki) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(data.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 1;
    c.server.flight.tls.cert = Some(pki.dir.join("server.pem"));
    c.server.flight.tls.key = Some(pki.dir.join("server.key"));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_trusts_the_ca_connects_and_one_that_does_not_is_refused() {
    let pki_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let pki = mint(pki_dir.path());

    let mut cfg = base_config(data.path(), &pki);
    cfg.auth.anonymous = yesno_server::config::Anonymous::Read;
    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;

    // Trusting the right CA, verifying the name the certificate carries.
    let ok = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca_pem.clone()))
        .domain_name("localhost");
    let mut c = client(addr, ok)
        .await
        .expect("a trusting client must connect");
    let info = c.get_flight_info(descriptor(1)).await.unwrap().into_inner();
    assert_eq!(info.total_records, 0);

    // A *different* CA, not a malformed one. This is the case that matters:
    // a well-formed certificate from an authority the client does not trust must
    // be refused, and that is what stops a network attacker presenting their own.
    let other_dir = tempfile::tempdir().unwrap();
    let other = mint(other_dir.path());
    let wrong = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(other.ca_pem))
        .domain_name("localhost");
    // Some stacks fail in `connect`, others defer the handshake to the first
    // call — both are a refusal, and the test must accept either.
    if let Ok(mut c) = client(addr, wrong).await {
        assert!(
            c.get_flight_info(descriptor(1)).await.is_err(),
            "a client trusting the wrong CA was served"
        );
    }

    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutual_tls_identifies_the_client_by_certificate_fingerprint() {
    let pki_dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let pki = mint(pki_dir.path());
    std::fs::write(pki_dir.path().join("client-ca.pem"), &pki.ca_pem).unwrap();

    let mut cfg = base_config(data.path(), &pki);
    cfg.server.flight.tls.client_ca = Some(pki_dir.path().join("client-ca.pem"));
    cfg.server.flight.tls.require_client_auth = true;
    // The fingerprint is what names the principal — not the CN in the
    // certificate, which nothing here parses.
    cfg.auth
        .principals
        .push(yesno_server::config::PrincipalConfig {
            name: "standby-b".into(),
            role: yesno_server::config::PrincipalRole::Writer,
            token_sha256: None,
            cert_sha256: Some(pki.client_fingerprint.clone()),
        });
    cfg.validate(false).unwrap();

    let running = yesno_server::start(&cfg).await.unwrap();
    let addr = running.flight_addr;

    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca_pem.clone()))
        .identity(Identity::from_pem(
            pki.client_cert_pem.clone(),
            pki.client_key_pem.clone(),
        ))
        .domain_name("localhost");
    let mut c = client(addr, tls)
        .await
        .expect("the client cert must be accepted");

    // Reaching `handshake` at all proves the certificate resolved to a
    // principal: the interceptor refuses anything it cannot name.
    let mut hs = c
        .handshake(futures::stream::iter(
            Vec::<arrow_flight::HandshakeRequest>::new(),
        ))
        .await
        .expect("handshake must succeed for an identified client")
        .into_inner();
    use futures::StreamExt;
    let first = hs.next().await.unwrap().unwrap();
    let body = String::from_utf8_lossy(&first.payload).into_owned();
    assert!(
        body.contains("standby-b"),
        "whoami did not name the client: {body}"
    );
    assert!(body.contains("Writer"), "{body}");

    // And a client with **no** certificate is refused by the transport, not
    // by the policy: `require_client_auth` is a TLS-level demand.
    let anon = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(pki.ca_pem.clone()))
        .domain_name("localhost");
    if let Ok(mut c) = client(addr, anon).await {
        assert!(
            c.get_flight_info(descriptor(1)).await.is_err(),
            "a client with no certificate was served under require_client_auth"
        );
    }

    drop(c);
    running.shutdown().await;
}

/// The half-configured shapes, which look like TLS and are not.
#[test]
fn a_half_configured_identity_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut base = Config::default();
    base.server.data_dir = Some(dir.path().to_path_buf());

    let mut c = base.clone();
    c.server.flight.tls.cert = Some("/x/cert.pem".into());
    assert!(
        c.validate(false).is_err(),
        "a cert with no key was accepted"
    );

    let mut c = base.clone();
    c.server.flight.tls.key = Some("/x/key.pem".into());
    assert!(
        c.validate(false).is_err(),
        "a key with no cert was accepted"
    );

    // Mutual TLS is still TLS: asking clients for certificates while having
    // none of your own cannot work.
    let mut c = base.clone();
    c.server.flight.tls.client_ca = Some("/x/ca.pem".into());
    assert!(c.validate(false).is_err());

    // And demanding client auth with nothing to verify against would refuse
    // every client — a configuration whose only effect is an outage.
    let mut c = base.clone();
    c.server.flight.tls.cert = Some("/x/cert.pem".into());
    c.server.flight.tls.key = Some("/x/key.pem".into());
    c.server.flight.tls.require_client_auth = true;
    assert!(c.validate(false).is_err());
}

/// A public bind needs **both** transport security and a credential.
///
/// TLS alone is not enough, and that is the point of this test. An encrypted
/// channel to a server that will do anything for anybody is a private
/// conversation with a stranger; the two checks are independent and this pins
/// that they are.
#[test]
fn a_public_bind_needs_tls_and_a_principal() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.server.data_dir = Some(dir.path().to_path_buf());
    c.server.flight.listen = "0.0.0.0:50051".into();

    // Neither.
    let e = c.validate(false).unwrap_err();
    assert!(format!("{e}").contains("refusing to bind"), "{e}");

    // TLS only: still refused, now for the other reason.
    c.server.flight.tls.cert = Some("/x/cert.pem".into());
    c.server.flight.tls.key = Some("/x/key.pem".into());
    let e = c.validate(false).unwrap_err();
    assert!(
        format!("{e}").contains("auth.principal"),
        "TLS alone was accepted for a public bind: {e}"
    );

    // Both.
    c.auth
        .principals
        .push(yesno_server::config::PrincipalConfig {
            name: "someone".into(),
            role: yesno_server::config::PrincipalRole::Reader,
            token_sha256: Some("0".repeat(64)),
            cert_sha256: None,
        });
    c.validate(false)
        .expect("TLS plus a principal must permit a public bind");

    // And `--insecure` is still the operator's override for a trusted network.
    let mut bare = Config::default();
    bare.server.data_dir = Some(dir.path().to_path_buf());
    bare.server.flight.listen = "0.0.0.0:50051".into();
    bare.validate(true).expect("--insecure must still work");
}

/// A certificate the server cannot read must fail at startup, loudly.
///
/// Not at the first handshake. A server that starts and then refuses every
/// connection is an outage that looks like a client problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_certificate_fails_at_startup() {
    let data = tempfile::tempdir().unwrap();
    let mut c = Config::default();
    c.server.data_dir = Some(data.path().to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.flight.tls.cert = Some(data.path().join("absent.pem"));
    c.server.flight.tls.key = Some(data.path().join("absent.key"));

    let msg = match yesno_server::start(&c).await {
        Ok(_) => panic!("the server started with a certificate it cannot read"),
        Err(e) => format!("{e}"),
    };
    assert!(msg.contains("cannot read the TLS"), "{msg}");
}
