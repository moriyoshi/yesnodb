//! The metrics listener must survive a role change.
//!
//! **The bug this pins down.** A follower and a leader render different
//! `/metrics` and answer `/readyz` differently, and both bind the same
//! `metrics_addr`. Serving them from two listeners made promotion a
//! stop-then-rebind: the port stayed closed for the whole of the follower's
//! close plus the leader's open, WAL replay and checkpoint load included.
//! Kubernetes points its liveness probe at that port with
//! `periodSeconds: 10, failureThreshold: 3`, so ~30 s of the window killed the
//! container -- during a promotion, which then lost the promotion.
//!
//! `/healthz` is a static `"ok\n"` that never consults the database, so the
//! only way it can fail is if the socket is gone. That is what makes a health
//! poller a sound detector here and why these tests are phrased around one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use yesno_server::follower::FollowerStatus;
use yesno_server::maintenance::Counters;
use yesno_server::metrics::{serve_shared, Shared};

/// Counts of what a `/healthz` poller saw.
struct Seen {
    ok: AtomicUsize,
    refused: AtomicUsize,
}

/// Poll `/healthz` until told to stop, counting answers and refusals.
fn poll(addr: std::net::SocketAddr) -> (Arc<Seen>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let seen = Arc::new(Seen {
        ok: AtomicUsize::new(0),
        refused: AtomicUsize::new(0),
    });
    let stop = Arc::new(AtomicUsize::new(0));
    let handle = tokio::spawn({
        let seen = seen.clone();
        let stop = stop.clone();
        async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while stop.load(Ordering::Relaxed) == 0 {
                match tokio::net::TcpStream::connect(addr).await {
                    Ok(mut s) => {
                        let req = "GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n";
                        if s.write_all(req.as_bytes()).await.is_ok() {
                            let mut buf = String::new();
                            if s.read_to_string(&mut buf).await.is_ok() && buf.contains(" 200 ") {
                                seen.ok.fetch_add(1, Ordering::Relaxed);
                            } else {
                                seen.refused.fetch_add(1, Ordering::Relaxed);
                            }
                        } else {
                            seen.refused.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Err(_) => {
                        seen.refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    });
    (seen, stop, handle)
}

/// The property: every role change is invisible to the probe.
#[tokio::test]
async fn a_role_change_never_closes_the_metrics_port() {
    let shared = Shared::new();
    let serving = serve_shared("127.0.0.1:0".parse().unwrap(), shared.clone())
        .await
        .unwrap();
    let addr = serving.addr;

    let (seen, stop, handle) = poll(addr);
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Starting -> Follower, as `follow()` does once the standby is up.
    let status = Arc::new(FollowerStatus::default());
    shared.set_follower(status.clone());
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Follower -> Leader, which is the promotion that used to rebind the port.
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(yesno_core::Db::open(dir.path()).unwrap());
    shared.set_leader(db, Arc::new(Counters::default()));
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Leader -> Follower. Demotion has the identical shape in reverse.
    shared.set_follower(status);
    tokio::time::sleep(Duration::from_millis(40)).await;

    stop.store(1, Ordering::Relaxed);
    handle.await.unwrap();

    assert_eq!(
        seen.refused.load(Ordering::Relaxed),
        0,
        "the port must answer through every role change"
    );
    assert!(
        seen.ok.load(Ordering::Relaxed) > 20,
        "the poller only made {} requests; it was not really watching",
        seen.ok.load(Ordering::Relaxed)
    );
    assert_eq!(serving.addr, addr, "the address must not move");
}

/// **Positive control.** Without this, the test above is green because the
/// poller cannot fail rather than because the port stayed up. This reproduces
/// the *old* shape -- stop, then rebind the same address -- and requires the
/// poller to notice.
#[tokio::test]
async fn a_stop_then_rebind_is_what_the_poller_is_able_to_catch() {
    let first = serve_shared("127.0.0.1:0".parse().unwrap(), Shared::new())
        .await
        .unwrap();
    let addr = first.addr;

    let (seen, stop, handle) = poll(addr);
    tokio::time::sleep(Duration::from_millis(40)).await;

    // The window: the listener is gone while the "role" changes over.
    first.stop().await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let second = serve_shared(addr, Shared::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;

    stop.store(1, Ordering::Relaxed);
    handle.await.unwrap();
    second.stop().await;

    assert!(
        seen.refused.load(Ordering::Relaxed) > 0,
        "the poller saw no failure across a real stop-then-rebind, so it cannot \
         detect the bug the other test claims to rule out"
    );
}
