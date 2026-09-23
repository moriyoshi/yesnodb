//! Write transactions across RPCs, through the real server.
//!
//! # Why this file exists rather than another case in `yesno-flight`
//!
//! `yesno-flight`'s own acceptance tests hold **one** `YesnoFlightService` and
//! call its methods directly, and the PostgreSQL and MySQL fixtures use an
//! in-process Flight service for the same reason. All of them passed while the
//! feature was completely broken against `yesnod`, because the bug was not in
//! the service at all: `GuardedFlight::current` built a *fresh* service per
//! RPC, so `begin_write` registered a transaction into an instance that was
//! dropped when the action returned and the next call looked in an empty table.
//!
//! No test that holds one service can see that. The discriminating property is
//! that **two different RPCs reach the same service state**, and the only place
//! to assert it is above the gRPC boundary. That is the whole point here.
//!
//! Ticket leasing had the identical bug and is not asserted here, because it
//! degrades invisibly -- `DoGet` falls back to re-opening by version and still
//! returns the right rows -- so there is no honest behavioural assertion to
//! make. It is fixed by the same change and recorded in the journal.

use std::path::Path;
use std::sync::Arc;

use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::{Action, FlightDescriptor};
use futures::StreamExt;
use yesno_server::config::{AuthzRule, Config, EndpointCapability, RuleAction};

fn config_for(dir: &Path) -> Config {
    let mut c = Config::default();
    c.server.data_dir = Some(dir.to_path_buf());
    c.server.flight.listen = "127.0.0.1:0".into();
    c.server.control.listen = "127.0.0.1:0".into();
    c.server.control.journal_dir = dir.join("control");
    c.server.metrics.listen = "127.0.0.1:0".into();
    c.server.shutdown_grace_secs = 10;
    c.db.shards = 2;
    c.auth.rules = vec![AuthzRule {
        channel: yesno_server::config::AuthzChannel::Host,
        principal: "all".into(),
        address: "127.0.0.0/8".into(),
        capability: EndpointCapability::ControlAdmin,
        action: RuleAction::Allow,
    }];
    c
}

async fn client(addr: std::net::SocketAddr) -> FlightServiceClient<tonic::transport::Channel> {
    let ch = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    FlightServiceClient::new(ch)
}

/// One `( key, lo, hi, op )` row per element.
fn mutations(rows: &[(u64, u64, u64, u8)]) -> arrow_array::RecordBatch {
    let schema = yesno_flight::mutations_schema();
    let keys = arrow_array::UInt64Array::from(rows.iter().map(|r| r.0).collect::<Vec<_>>());
    let lo = arrow_array::UInt64Array::from(rows.iter().map(|r| r.1).collect::<Vec<_>>());
    let hi = arrow_array::UInt64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>());
    let op = arrow_array::UInt8Array::from(rows.iter().map(|r| r.3).collect::<Vec<_>>());
    arrow_array::RecordBatch::try_new(
        schema,
        vec![Arc::new(keys), Arc::new(lo), Arc::new(hi), Arc::new(op)],
    )
    .unwrap()
}

async fn action(
    c: &mut FlightServiceClient<tonic::transport::Channel>,
    name: &str,
    body: Vec<u8>,
) -> Result<Vec<u8>, tonic::Status> {
    let mut stream = c
        .do_action(Action {
            r#type: name.into(),
            body: body.into(),
        })
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(r) = stream.next().await {
        out.extend_from_slice(&r?.body);
    }
    Ok(out)
}

async fn stage(
    c: &mut FlightServiceClient<tonic::transport::Channel>,
    txn: u64,
    rows: &[(u64, u64, u64, u8)],
) -> Result<(), tonic::Status> {
    let mut command = yesno_flight::PUT_TXN_PREFIX.to_vec();
    command.extend_from_slice(&txn.to_le_bytes());
    let batch = mutations(rows);
    let input = arrow_flight::encode::FlightDataEncoderBuilder::new()
        .with_schema(yesno_flight::mutations_schema())
        .with_flight_descriptor(Some(FlightDescriptor::new_cmd(command)))
        .build(futures::stream::iter(vec![Ok(batch)]))
        .map(|r| r.unwrap());
    let mut acks = c.do_put(input).await?.into_inner();
    while let Some(r) = acks.next().await {
        r?;
    }
    Ok(())
}

async fn cardinality(c: &mut FlightServiceClient<tonic::transport::Channel>, key: u64) -> i64 {
    c.get_flight_info(FlightDescriptor::new_cmd(key.to_le_bytes().to_vec()))
        .await
        .unwrap()
        .into_inner()
        .total_records
}

/// The assertion the in-process fixtures structurally cannot make.
///
/// Every step here is a separate RPC. Before the fix this failed at the first
/// `stage` with "write transaction 1 is not open" -- the handle the previous
/// call had just issued.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_transaction_survives_across_rpcs() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();
    let mut c = client(running.flight_addr).await;

    let begun = action(&mut c, yesno_flight::ACTION_BEGIN_WRITE, Vec::new())
        .await
        .unwrap();
    assert_eq!(begun.len(), 8, "begin_write returns one 8-byte handle");
    let txn = u64::from_le_bytes(begun.try_into().unwrap());

    stage(
        &mut c,
        txn,
        &[
            (7, 80, 80, yesno_flight::OP_INSERT),
            (7, 81, 81, yesno_flight::OP_INSERT),
        ],
    )
    .await
    .unwrap();
    // A second staging call into the same transaction: the state has now
    // outlived two RPCs, not one.
    stage(&mut c, txn, &[(7, 90, 92, yesno_flight::OP_INSERT_RANGE)])
        .await
        .unwrap();

    assert_eq!(
        cardinality(&mut c, 7).await,
        0,
        "staged rows must be invisible to a reader until commit"
    );

    let committed = action(
        &mut c,
        yesno_flight::ACTION_COMMIT_WRITE,
        txn.to_le_bytes().to_vec(),
    )
    .await
    .unwrap();
    let version = u64::from_le_bytes(committed.try_into().unwrap());
    assert!(version > 0);

    assert_eq!(
        cardinality(&mut c, 7).await,
        5,
        "everything staged becomes visible at one version"
    );

    // Commit is idempotent by identity, and that too is state held between
    // calls: a retry after a lost response returns the original version.
    let replayed = action(
        &mut c,
        yesno_flight::ACTION_COMMIT_WRITE,
        txn.to_le_bytes().to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(u64::from_le_bytes(replayed.try_into().unwrap()), version);

    running.shutdown().await;
}

/// Abort is the other half, and it also spans calls.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_transaction_applies_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let running = yesno_server::start(&config_for(dir.path())).await.unwrap();
    let mut c = client(running.flight_addr).await;

    let begun = action(&mut c, yesno_flight::ACTION_BEGIN_WRITE, Vec::new())
        .await
        .unwrap();
    let txn = u64::from_le_bytes(begun.try_into().unwrap());

    stage(&mut c, txn, &[(9, 90, 90, yesno_flight::OP_INSERT)])
        .await
        .unwrap();
    action(
        &mut c,
        yesno_flight::ACTION_ABORT_WRITE,
        txn.to_le_bytes().to_vec(),
    )
    .await
    .unwrap();

    assert_eq!(cardinality(&mut c, 9).await, 0);

    // Committing an aborted transaction fails rather than applying it late.
    let err = action(
        &mut c,
        yesno_flight::ACTION_COMMIT_WRITE,
        txn.to_le_bytes().to_vec(),
    )
    .await
    .expect_err("an aborted transaction cannot be committed");
    assert_eq!(err.code(), tonic::Code::NotFound);

    running.shutdown().await;
}
