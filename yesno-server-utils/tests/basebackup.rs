//! Base-backup publication and failure cleanup in the utility crate that owns it.
//!
//! This fake server deliberately truncates one leased file. The client must
//! reject the protocol violation, release the lease, and remove its staging
//! directory without ever publishing the target.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status};
use yesno_server::control::pb::control_plane_server::{ControlPlane, ControlPlaneServer};
use yesno_server::control::pb::*;
use yesno_server_utils::basebackup::base_backup;

#[derive(Clone)]
struct TruncatedSnapshot {
    released: Arc<AtomicBool>,
}

#[tonic::async_trait]
impl ControlPlane for TruncatedSnapshot {
    type SubscribeStream = ReceiverStream<Result<SubscribeItem, Status>>;
    type FetchSnapshotFileStream = ReceiverStream<Result<SnapshotFileChunk, Status>>;

    async fn subscribe(
        &self,
        _request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn get_snapshot(
        &self,
        _request: Request<GetSnapshotRequest>,
    ) -> Result<Response<StateSnapshot>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn checkpoint(
        &self,
        _request: Request<CheckpointRequest>,
    ) -> Result<Response<CheckpointResponse>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn promote(
        &self,
        _request: Request<PromoteRequest>,
    ) -> Result<Response<CommandAccepted>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn demote(
        &self,
        _request: Request<DemoteRequest>,
    ) -> Result<Response<CommandAccepted>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn shutdown(
        &self,
        _request: Request<ShutdownRequest>,
    ) -> Result<Response<CommandAccepted>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn begin_base_snapshot(
        &self,
        _request: Request<BeginBaseSnapshotRequest>,
    ) -> Result<Response<BaseSnapshotLease>, Status> {
        Ok(Response::new(BaseSnapshotLease {
            lease_id: b"lease".to_vec(),
            source: BaseSnapshotSource::Portable as i32,
            files: vec![SnapshotFile {
                name: "MANIFEST".into(),
                size: 8,
            }],
            lease_ttl_secs: 30,
            direct_path_available: false,
            deferred_ebs: None,
        }))
    }

    async fn fetch_snapshot_file(
        &self,
        _request: Request<FetchSnapshotFileRequest>,
    ) -> Result<Response<Self::FetchSnapshotFileStream>, Status> {
        let (send, receive) = tokio::sync::mpsc::channel(1);
        send.send(Ok(SnapshotFileChunk {
            offset: 0,
            payload: Some(snapshot_file_chunk::Payload::Data(b"short".to_vec())),
            last: false,
            total_size: 8,
        }))
        .await
        .unwrap();
        drop(send);
        Ok(Response::new(ReceiverStream::new(receive)))
    }

    async fn keep_base_snapshot_alive(
        &self,
        _request: Request<KeepBaseSnapshotAliveRequest>,
    ) -> Result<Response<BaseSnapshotLifetime>, Status> {
        Ok(Response::new(BaseSnapshotLifetime { lease_ttl_secs: 30 }))
    }

    async fn release_base_snapshot(
        &self,
        _request: Request<ReleaseBaseSnapshotRequest>,
    ) -> Result<Response<ReleaseBaseSnapshotResponse>, Status> {
        self.released.store(true, Ordering::Release);
        Ok(Response::new(ReleaseBaseSnapshotResponse {}))
    }

    async fn claim_snapshot_agent_work(
        &self,
        _request: Request<ClaimSnapshotAgentWorkRequest>,
    ) -> Result<Response<SnapshotAgentWork>, Status> {
        Err(Status::unimplemented("test service"))
    }

    async fn complete_snapshot_agent_work(
        &self,
        _request: Request<CompleteSnapshotAgentWorkRequest>,
    ) -> Result<Response<CompleteSnapshotAgentWorkResponse>, Status> {
        Err(Status::unimplemented("test service"))
    }
}

async fn serve(service: TruncatedSnapshot) -> (Channel, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        Server::builder()
            .add_service(ControlPlaneServer::new(service))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
            .unwrap();
    });
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    (channel, shutdown)
}

#[tokio::test]
async fn failed_stream_releases_lease_and_removes_partial_directory() {
    let destination = tempfile::tempdir().unwrap();
    let target = destination.path().join("base");
    let released = Arc::new(AtomicBool::new(false));
    let (channel, shutdown) = serve(TruncatedSnapshot {
        released: released.clone(),
    })
    .await;

    let error = base_backup(channel, &target)
        .await
        .expect_err("the truncated snapshot must not be published");
    assert!(
        error.to_string().contains("before its last marker"),
        "{error}"
    );
    assert!(released.load(Ordering::Acquire), "lease was not released");
    assert!(!target.exists());
    assert_eq!(
        std::fs::read_dir(destination.path()).unwrap().count(),
        0,
        "failed transfer left a partial directory behind"
    );
    let _ = shutdown.send(());
}
