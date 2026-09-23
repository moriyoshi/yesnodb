//! The authorization table, and the distinction between "who are you" and
//! "you may not".
//!
//! The matrix crosses Flight reader/writer roles with the independent control
//! capability that permits a checkpoint. A replica credential must be able to
//! ship WAL without thereby gaining either data access or administrative I/O.
//!
//! **And the matrix is sabotage-checked.** A permission table that is 90%
//! right reads as right — the same argument `scripts/check-layout.py`'s header
//! makes about diagrams. `every_role_is_refused_what_it_must_not_have` walks the
//! whole grid rather than spot-checking it, so flipping one entry in `guard.rs`
//! reddens a named case.

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::{Action, FlightDescriptor};
use futures::StreamExt;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;
use tonic::{Code, Request, Status};
use yesno_server::config::{
    Anonymous, AuthzRule, Config, EndpointCapability, PrincipalConfig, PrincipalRole, RuleAction,
};

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let d = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::new();
    for b in d.as_ref() {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[derive(Clone)]
struct Bearer {
    token: Option<String>,
    expect_term: Option<u32>,
}

impl Bearer {
    fn expecting(term: u32) -> Bearer {
        Bearer {
            token: None,
            expect_term: Some(term),
        }
    }
}

impl tonic::service::Interceptor for Bearer {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        if let Some(t) = &self.token {
            req.metadata_mut()
                .insert("authorization", format!("Bearer {t}").parse().unwrap());
        }
        if let Some(n) = self.expect_term {
            req.metadata_mut().insert(
                yesno_server::guard::EXPECT_TERM,
                n.to_string().parse().unwrap(),
            );
        }
        Ok(req)
    }
}

type Client = FlightServiceClient<InterceptedService<Channel, Bearer>>;
type ControlClient = yesno_server::control::pb::control_plane_client::ControlPlaneClient<
    InterceptedService<Channel, Bearer>,
>;

/// Sends whatever it is given as the expected-term header, valid or not.
#[derive(Clone)]
struct RawHeader(String);

impl tonic::service::Interceptor for RawHeader {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.metadata_mut()
            .insert(yesno_server::guard::EXPECT_TERM, self.0.parse().unwrap());
        Ok(req)
    }
}

async fn client_expecting(addr: std::net::SocketAddr, term: u32) -> Client {
    let ch = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    FlightServiceClient::with_interceptor(ch, Bearer::expecting(term))
}

async fn try_write_expecting(c: &mut Client) -> Result<(), Code> {
    try_write(c).await
}

async fn client(addr: std::net::SocketAddr, token: Option<&str>) -> Client {
    let ch = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    FlightServiceClient::with_interceptor(
        ch,
        Bearer {
            token: token.map(|s| s.to_owned()),
            expect_term: None,
        },
    )
}

async fn control_client(addr: std::net::SocketAddr, token: Option<&str>) -> ControlClient {
    let ch = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    yesno_server::control::pb::control_plane_client::ControlPlaneClient::with_interceptor(
        ch,
        Bearer {
            token: token.map(str::to_owned),
            expect_term: None,
        },
    )
}

fn principal(name: &str, role: PrincipalRole, token: &str) -> PrincipalConfig {
    PrincipalConfig {
        name: name.into(),
        role,
        token_sha256: Some(sha256_hex(token.as_bytes())),
        cert_sha256: None,
    }
}

fn config(dir: &std::path::Path, anonymous: Anonymous) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(dir.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.control.listen = "127.0.0.1:0".into();
    c.server.control.journal_dir = dir.join("control");
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 1;
    c.auth.anonymous = anonymous;
    c.auth.principals = vec![
        principal("reader", PrincipalRole::Reader, "r-tok"),
        principal("writer", PrincipalRole::Writer, "w-tok"),
        principal("admin", PrincipalRole::Admin, "a-tok"),
        principal("standby", PrincipalRole::Replica, "s-tok"),
    ];
    c.auth.rules = vec![AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "admin".into(),
        address: "127.0.0.0/8".into(),
        capability: EndpointCapability::ControlAdmin,
        action: RuleAction::Allow,
    }];
    c
}

fn descriptor(key: u64) -> FlightDescriptor {
    FlightDescriptor::new_cmd(key.to_le_bytes().to_vec())
}

async fn try_read(c: &mut Client) -> Result<(), Code> {
    c.get_flight_info(descriptor(1))
        .await
        .map(|_| ())
        .map_err(|e| e.code())
}

async fn try_write(c: &mut Client) -> Result<(), Code> {
    let schema = yesno_flight::pairs_schema();
    let batch = arrow_array::RecordBatch::try_new(
        schema.clone(),
        vec![
            std::sync::Arc::new(arrow_array::UInt64Array::new(vec![1u64].into(), None)),
            std::sync::Arc::new(arrow_array::UInt64Array::new(vec![7u64].into(), None)),
        ],
    )
    .unwrap();
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(schema)
        // `do_put` requires an explicit command; it no longer defaults
        // to insert, so that an unrecognised one cannot be applied as one.
        .with_flight_descriptor(Some(arrow_flight::FlightDescriptor::new_cmd(
            yesno_flight::PUT_INSERT.to_vec(),
        )))
        .build(futures::stream::iter(vec![Ok(batch)]))
        .map(|r| r.unwrap());
    match c.do_put(input).await {
        Err(e) => Err(e.code()),
        Ok(resp) => {
            // The status can arrive on the stream rather than on the call, so
            // draining is part of asking. A test that stopped at `Ok` here would
            // report a refused write as permitted.
            let mut s = resp.into_inner();
            while let Some(r) = s.next().await {
                if let Err(e) = r {
                    return Err(e.code());
                }
            }
            Ok(())
        }
    }
}

async fn try_action(c: &mut Client, name: &str) -> Result<(), Code> {
    match c
        .do_action(Action {
            r#type: name.into(),
            body: Default::default(),
        })
        .await
    {
        Err(e) => Err(e.code()),
        Ok(resp) => {
            let mut s = resp.into_inner();
            while let Some(r) = s.next().await {
                if let Err(e) = r {
                    return Err(e.code());
                }
            }
            Ok(())
        }
    }
}

async fn try_checkpoint(c: &mut ControlClient) -> Result<(), Code> {
    c.checkpoint(yesno_server::control::pb::CheckpointRequest {})
        .await
        .map(|_| ())
        .map_err(|error| error.code())
}

/// The whole grid, in one place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_role_is_refused_what_it_must_not_have() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::None))
        .await
        .unwrap();
    let addr = running.flight_addr;
    let control_addr = running.control_addr.unwrap();

    // ( token, may read, may write, may admin )
    let grid = [
        ("r-tok", true, false, false),
        ("w-tok", true, true, false),
        ("a-tok", true, true, true),
        // The disjoint role: a replication credential ships a WAL and must
        // not thereby be able to read a key or force a checkpoint.
        ("s-tok", false, false, false),
    ];

    for (token, may_read, may_write, may_admin) in grid {
        let mut c = client(addr, Some(token)).await;
        let mut control = control_client(control_addr, Some(token)).await;

        assert_eq!(
            try_read(&mut c).await.is_ok(),
            may_read,
            "{token}: read expected {may_read}"
        );
        assert_eq!(
            try_write(&mut c).await.is_ok(),
            may_write,
            "{token}: write expected {may_write}"
        );
        // `stats` is a Flight read; checkpoint is an independent control
        // capability rather than a Flight action.
        assert_eq!(
            try_action(&mut c, "stats").await.is_ok(),
            may_read,
            "{token}: do_action( \"stats\" ) expected {may_read}"
        );
        assert_eq!(
            try_checkpoint(&mut control).await.is_ok(),
            may_admin,
            "{token}: checkpoint expected {may_admin}"
        );
    }

    running.shutdown().await;
}

/// 16 and 7 are different answers and a client acts on the difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_and_permission_denied_are_not_the_same_answer() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::None))
        .await
        .unwrap();
    let addr = running.flight_addr;

    // No credential at all: the server cannot name the caller.
    let mut anon = client(addr, None).await;
    assert_eq!(try_read(&mut anon).await, Err(Code::Unauthenticated));

    // A credential that is not one we know. Also `unauthenticated`, and
    // deliberately not a silent downgrade to anonymous — a caller who presents
    // a token is asserting an identity.
    let mut bogus = client(addr, Some("not-a-token")).await;
    assert_eq!(try_read(&mut bogus).await, Err(Code::Unauthenticated));

    // A credential we do know, for something it may not do. The caller proved
    // who they are; retrying will never help, and the code has to say so.
    let mut reader = client(addr, Some("r-tok")).await;
    assert_eq!(try_write(&mut reader).await, Err(Code::PermissionDenied));
    assert_eq!(
        try_action(&mut reader, "clear").await,
        Err(Code::PermissionDenied)
    );

    running.shutdown().await;
}

/// The public-read-replica shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_read_permits_reads_and_still_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();
    let addr = running.flight_addr;

    let mut c = client(addr, None).await;
    assert!(try_read(&mut c).await.is_ok(), "anonymous read was refused");
    assert!(try_action(&mut c, "stats").await.is_ok());
    assert_eq!(try_write(&mut c).await, Err(Code::PermissionDenied));
    assert_eq!(
        try_action(&mut c, "clear").await,
        Err(Code::PermissionDenied)
    );

    running.shutdown().await;
}

/// An action this build does not know must be **admin**, not permitted.
///
/// Default-deny by shape. When a new action is added to `yesno-flight`, it
/// lands in the strictest bucket until somebody deliberately moves it — rather
/// than inheriting whatever the fall-through happened to be, which is how a new
/// verb quietly becomes world-writable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_action_needs_admin() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();
    let addr = running.flight_addr;

    let mut reader = client(addr, Some("r-tok")).await;
    assert_eq!(
        try_action(&mut reader, "some-future-action").await,
        Err(Code::PermissionDenied)
    );

    // An admin gets past the guard and is refused by the *service*, which does
    // not know the action — a different failure, and the right one.
    let mut admin = client(addr, Some("a-tok")).await;
    assert_eq!(
        try_action(&mut admin, "some-future-action").await,
        Err(Code::InvalidArgument)
    );

    running.shutdown().await;
}

/// `handshake` is a whoami probe: it needs a credential and no permission.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_reports_the_caller_and_needs_only_a_credential() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::None))
        .await
        .unwrap();
    let addr = running.flight_addr;

    // Even the replica role, which may do nothing else at all, can ask who it is.
    for (token, expect) in [("s-tok", "standby"), ("r-tok", "reader")] {
        let mut c = client(addr, Some(token)).await;
        let mut s = c
            .handshake(futures::stream::iter(
                Vec::<arrow_flight::HandshakeRequest>::new(),
            ))
            .await
            .expect("an identified caller may ask who it is")
            .into_inner();
        let body = s.next().await.unwrap().unwrap().payload;
        let body = String::from_utf8_lossy(&body).into_owned();
        assert!(body.contains(expect), "{token}: {body}");
        assert!(body.contains("yesnod/"), "{body}");
    }

    // Without one, it is the same `unauthenticated` as everything else.
    let mut anon = client(addr, None).await;
    let e = anon
        .handshake(futures::stream::iter(
            Vec::<arrow_flight::HandshakeRequest>::new(),
        ))
        .await
        .unwrap_err();
    assert_eq!(e.code(), Code::Unauthenticated);

    running.shutdown().await;
}

// ---------------------------------------------------------------------------
// Client-side fencing
// ---------------------------------------------------------------------------

/// A caller that has seen a newer leadership must be refused by an older one.
///
/// **This is the half of split-brain fencing that faces clients.** A
/// superseded leader does not know it has been replaced — nothing tells it — so
/// it accepts writes that vanish when it is wiped and rebuilt. The caller is the
/// only party that can hold the newer number, which is why the assertion travels
/// on the request rather than being something the server could check alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_expecting_a_newer_term_is_refused_by_an_older_leader() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();
    let addr = running.flight_addr;

    // The server is a fresh database, so term 0.
    let mut plain = client(addr, None).await;
    let resp = plain.list_actions(arrow_flight::Empty {}).await.unwrap();
    assert_eq!(
        resp.metadata()
            .get(yesno_server::guard::TERM)
            .and_then(|v| v.to_str().ok()),
        Some("0"),
        "the server did not report its term"
    );

    // A caller that has seen term 1 must not be served by it.
    let mut ahead = client_expecting(addr, 1).await;
    let e = ahead
        .get_flight_info(descriptor(1))
        .await
        .expect_err("a superseded leader served a caller that knows better");
    assert_eq!(e.code(), Code::FailedPrecondition, "{e}");
    assert!(e.message().contains("older leadership"), "{}", e.message());

    // Reads **and** writes. A stale read is less catastrophic than a lost
    // write and is still an answer from a timeline that has been abandoned.
    let mut ahead = client_expecting(addr, 1).await;
    assert_eq!(
        try_write_expecting(&mut ahead).await,
        Err(Code::FailedPrecondition)
    );

    // And a caller *behind* the server is fine. After a failover every client
    // is behind for a moment; refusing them would turn a promotion into an
    // outage. The response header is what teaches them the newer number.
    let mut behind = client_expecting(addr, 0).await;
    assert!(behind.get_flight_info(descriptor(1)).await.is_ok());

    running.shutdown().await;
}

/// The whole point, end to end: promote, and the old leader stops serving the
/// clients that have moved on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_promotion_makes_the_old_leader_refuse_clients_that_have_moved_on() {
    let dir = tempfile::tempdir().unwrap();

    // ---- the original leadership
    let old = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();
    let old_addr = old.flight_addr;
    let mut c = client_expecting(old_addr, 0).await;
    assert!(c.get_flight_info(descriptor(1)).await.is_ok());

    // ---- a failover elsewhere raises the term to 1, and this client learns it.
    //
    // The old server keeps running at term 0 — that is the zombie.
    let e = client_expecting(old_addr, 1)
        .await
        .get_flight_info(descriptor(1))
        .await
        .expect_err("the client was served by the leadership it had moved on from");
    assert_eq!(e.code(), Code::FailedPrecondition);

    old.shutdown().await;

    // ---- and a genuinely promoted server does serve that client
    yesno_server::lifecycle::raise_term(dir.path()).unwrap();
    let new = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();
    let mut c = client_expecting(new.flight_addr, 1).await;
    let resp = c.list_actions(arrow_flight::Empty {}).await.unwrap();
    assert_eq!(
        resp.metadata()
            .get(yesno_server::guard::TERM)
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    assert!(c.get_flight_info(descriptor(1)).await.is_ok());

    new.shutdown().await;
}

/// A malformed expectation is an argument error, not a silent pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_expected_term_is_refused_rather_than_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config(dir.path(), Anonymous::Read))
        .await
        .unwrap();

    let ch = Channel::from_shared(format!("http://{}", running.flight_addr))
        .unwrap()
        .connect()
        .await
        .unwrap();
    let mut c = FlightServiceClient::with_interceptor(ch, RawHeader("not-a-number".into()));
    let e = c
        .get_flight_info(descriptor(1))
        .await
        .expect_err("a header that is not a number was accepted");
    // `InvalidArgument`, not a quiet skip. A client that meant to fence itself
    // and typo'd the value must not be served as though it had asked for nothing.
    assert_eq!(e.code(), Code::InvalidArgument, "{e}");

    running.shutdown().await;
}
