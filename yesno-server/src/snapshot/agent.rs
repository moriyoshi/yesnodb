//! Server-side queue for privileged local snapshot work.
//!
//! The queue carries an operation, a database namespace, and a lease name —
//! and deliberately nothing else. Every path, device, and volume the privileged
//! side touches is derived from the agent's *own* copy of the configuration or
//! looked up from an authoritative source, never taken from the daemon. That is
//! what keeps a compromised daemon from turning the agent into a general-purpose
//! "mount this for me" service, and it is why `validate_work` lives here rather
//! than in one backend: both executors must apply it before acting.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};

use super::SnapshotError;
use crate::control::pb;

const QUEUE_DEPTH: usize = 64;

type AgentCompletion = Result<AgentReply, String>;
type CompletionSender = oneshot::Sender<AgentCompletion>;

#[derive(Clone)]
pub(crate) struct SnapshotAgentBroker {
    inner: Arc<Inner>,
}

struct Inner {
    sender: mpsc::Sender<pb::SnapshotAgentWork>,
    receiver: tokio::sync::Mutex<mpsc::Receiver<pb::SnapshotAgentWork>>,
    pending: Mutex<HashMap<Vec<u8>, CompletionSender>>,
    next_id: AtomicU64,
    closed: watch::Sender<bool>,
}

struct PendingOperation {
    inner: Arc<Inner>,
    operation_id: Vec<u8>,
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        self.inner
            .pending
            .lock()
            .unwrap()
            .remove(&self.operation_id);
    }
}

#[derive(Debug)]
pub(crate) struct AgentReply {
    pub(crate) root: String,
}

/// Reject work whose namespace or lease name the agent did not expect.
///
/// The lease name is the only daemon-supplied value any executor may use to
/// build a path or match a resource, so it must be proved to sit inside the
/// database namespace before it is used for either.
pub(super) fn validate_work(
    operation: pb::SnapshotAgentOperation,
    namespace: &str,
    name: &str,
) -> Result<(), SnapshotError> {
    if !valid_namespace(namespace) {
        return Err("snapshot agent received an invalid database namespace".into());
    }
    if operation == pb::SnapshotAgentOperation::Reconcile {
        if !name.is_empty() {
            return Err("snapshot reconciliation must not carry a lease name".into());
        }
    } else if !owned_name(namespace, name) {
        return Err("snapshot agent received a lease outside the database namespace".into());
    }
    Ok(())
}

fn valid_namespace(namespace: &str) -> bool {
    namespace
        .strip_prefix("yesno-snapshot-")
        .is_some_and(|uuid| {
            uuid.len() == 32
                && uuid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

pub(super) fn owned_name(namespace: &str, name: &str) -> bool {
    name.strip_prefix(namespace).is_some_and(|suffix| {
        suffix.starts_with('-')
            && suffix.len() > 1
            && suffix[1..]
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

impl SnapshotAgentBroker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) = mpsc::channel(QUEUE_DEPTH);
        let (closed, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                sender,
                receiver: tokio::sync::Mutex::new(receiver),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                closed,
            }),
        }
    }

    pub(crate) async fn execute(
        &self,
        operation: pb::SnapshotAgentOperation,
        namespace: &str,
        lease_name: &str,
        timeout: Duration,
    ) -> Result<AgentReply, SnapshotError> {
        let operation_id = self.operation_id();
        let (sender, receiver) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap()
            .insert(operation_id.clone(), sender);
        let pending = PendingOperation {
            inner: self.inner.clone(),
            operation_id: operation_id.clone(),
        };
        let work = pb::SnapshotAgentWork {
            operation_id: operation_id.clone(),
            operation: operation as i32,
            namespace: namespace.to_owned(),
            lease_name: lease_name.to_owned(),
        };
        if self.inner.sender.send(work).await.is_err() {
            return Err("snapshot-agent work queue is closed".into());
        }
        let result = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(Ok(reply))) => Ok(reply),
            Ok(Ok(Err(error))) => Err(error.into()),
            Ok(Err(_)) => Err("snapshot-agent completion channel closed".into()),
            Err(_) => Err(format!(
                "snapshot-agent {:?} operation timed out after {} seconds",
                operation,
                timeout.as_secs()
            )
            .into()),
        };
        drop(pending);
        result
    }

    pub(crate) async fn claim(&self) -> Result<pb::SnapshotAgentWork, SnapshotError> {
        let mut closed = self.inner.closed.subscribe();
        if *closed.borrow() {
            return Err("snapshot-agent work queue is closed".into());
        }
        let mut receiver = self.inner.receiver.lock().await;
        tokio::select! {
            work = receiver.recv() => work.ok_or_else(|| "snapshot-agent work queue is closed".into()),
            result = closed.changed() => {
                let _ = result;
                Err("snapshot-agent work queue is closed".into())
            }
        }
    }

    pub(crate) fn close(&self) {
        self.inner.closed.send_replace(true);
    }

    pub(crate) fn complete(
        &self,
        request: pb::CompleteSnapshotAgentWorkRequest,
    ) -> Result<(), SnapshotError> {
        let sender = self
            .inner
            .pending
            .lock()
            .unwrap()
            .remove(&request.operation_id)
            .ok_or("snapshot-agent operation does not exist or already completed")?;
        let reply = if request.success {
            Ok(AgentReply { root: request.root })
        } else if request.error.is_empty() {
            Err("snapshot agent reported failure without a reason".to_owned())
        } else {
            Err(request.error)
        };
        sender
            .send(reply)
            .map_err(|_| "snapshot-agent operation is no longer waiting".into())
    }

    fn operation_id(&self) -> Vec<u8> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let next = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{now:032x}-{next:016x}").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_failure_is_returned_to_the_waiting_operation() {
        let broker = SnapshotAgentBroker::new();
        let operation = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .execute(
                        pb::SnapshotAgentOperation::Cleanup,
                        "yesno-snapshot-0123456789abcdef0123456789abcdef",
                        "lease",
                        Duration::from_secs(1),
                    )
                    .await
            })
        };
        let work = broker.claim().await.unwrap();
        broker
            .complete(pb::CompleteSnapshotAgentWorkRequest {
                operation_id: work.operation_id,
                success: false,
                error: "injected privileged failure".into(),
                root: String::new(),
            })
            .unwrap();
        assert_eq!(
            operation.await.unwrap().unwrap_err().to_string(),
            "injected privileged failure"
        );
    }

    #[tokio::test]
    async fn closing_the_broker_releases_a_blocked_claim() {
        let broker = SnapshotAgentBroker::new();
        let claim = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.claim().await })
        };
        tokio::task::yield_now().await;
        broker.close();
        assert_eq!(
            claim.await.unwrap().unwrap_err().to_string(),
            "snapshot-agent work queue is closed"
        );
    }
}
