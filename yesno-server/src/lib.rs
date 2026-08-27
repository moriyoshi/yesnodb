//! `yesnod` — the yesno database daemon — and the pieces `yesno` shares
//! with it.
//!
//! # Why this is a separate crate
//!
//! `yesno-core` may not gain an async runtime or a gRPC stack: the `lean-core`
//! CI job fails if `tokio`, `tonic`, `prost` or `arrow-flight` appear in
//! `cargo tree -p yesno-core`, and caps its direct dependencies at five. Every
//! network surface therefore depends on core and never the reverse. The daemon
//! owns replication because it also owns the authenticated listener, role
//! transitions, retention registration, and snapshot coordination.
//!
//! In `members` but **not** `default-members`, following `yesno-datafusion`:
//! a plain `cargo test` stays fast while the routine gate and CI use
//! `--workspace` and cover this crate.
//!
//! # What this build does and does not do
//!
//! Arrow Flight, optionally over TLS ( [`tls`] ), behind an authentication
//! interceptor ( [`auth`] ) and a per-RPC authorization wrapper ( [`guard`] ).
//! A second gRPC endpoint combines lifecycle/event control with WAL and image
//! replication. It uses the same authenticated principal resolver plus an
//! ordered, first-match authorization table for those distinct capabilities.
//! The daemon can run as a cold or read-serving follower. TLS and authentication are
//! *optional*, so a plaintext, anonymous, loopback deployment is still one line
//! of configuration. That is why [`config::Config::validate`] refuses a
//! **non-loopback** bind with no principals unless the operator explicitly
//! passes `--insecure`. Structured `tracing` output always remains local; an
//! opt-in OpenTelemetry SDK provider can additionally batch sampled spans to an
//! OTLP/gRPC collector and is flushed during daemon shutdown.
//!
//! `do_put` ingests, so an open Flight port is an open write surface. Admin
//! operations live exclusively on the authenticated control plane.
//!
//! Still absent, and worth naming rather than implying: certificate hot
//! reload ( restart to rotate ), automatic failover or leader election, and
//! synchronous replication.
//!
//! Point-in-time recovery is answered by the **archive**, not by this
//! daemon: `yesnoctl restore` reconstructs a directory at a commit version or a
//! wall clock from a published object-store history. `yesnod` contributes the
//! commit times that make a wall-clock target answerable and nothing else, so a
//! deployment without an archive sidecar has no recovery points to target.

pub mod auth;
pub mod config;
pub mod control;
pub mod follower;
pub mod guard;
pub mod lifecycle;
pub mod local_transport;
pub mod maintenance;
pub mod metrics;
pub mod replication;
pub mod snapshot;
pub mod tls;

pub use config::{Config, ConfigError};
pub use lifecycle::{start, Running, Teardown};

/// A tracing installation that owns the OpenTelemetry provider, when enabled.
///
/// Keep this value alive for the daemon lifetime and call [`TelemetryGuard::shutdown`]
/// after request handling stops so the batch processor flushes pending spans.
pub struct TelemetryGuard {
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

/// Errors while constructing, installing, or flushing daemon telemetry.
pub type TelemetryError = Box<dyn std::error::Error + Send + Sync + 'static>;

impl TelemetryGuard {
    fn shutdown_inner(&mut self) -> Result<(), TelemetryError> {
        let Some(provider) = self.provider.take() else {
            return Ok(());
        };
        provider
            .shutdown()
            .map_err(|error| Box::new(error) as TelemetryError)
    }

    /// Flush pending spans and stop the SDK's batch processor.
    pub fn shutdown(mut self) -> Result<(), TelemetryError> {
        self.shutdown_inner()
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn sampler(config: &config::TelemetryConfig) -> opentelemetry_sdk::trace::Sampler {
    use config::TraceSampler;
    use opentelemetry_sdk::trace::Sampler;

    let ratio = || Sampler::TraceIdRatioBased(config.sample_ratio);
    match config.sampler {
        TraceSampler::AlwaysOn => Sampler::AlwaysOn,
        TraceSampler::AlwaysOff => Sampler::AlwaysOff,
        TraceSampler::TraceIdRatio => ratio(),
        TraceSampler::ParentBasedAlwaysOn => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        TraceSampler::ParentBasedAlwaysOff => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        TraceSampler::ParentBasedTraceIdRatio => Sampler::ParentBased(Box::new(ratio())),
    }
}

/// Install local formatted tracing and optional OTLP/gRPC trace export.
///
/// When export is enabled, call this while a Tokio runtime is entered and keep
/// that runtime alive until the returned guard is shut down.
///
/// The console filter applies only to the formatting layer. OpenTelemetry sees
/// every span and lets the configured SDK sampler decide which traces to record
/// and export.
pub fn init_tracing_with_telemetry(
    filter: &str,
    config: &config::TelemetryConfig,
) -> Result<TelemetryGuard, TelemetryError> {
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::{InstrumentationScope, KeyValue};
    use opentelemetry_otlp::WithExportConfig;
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{EnvFilter, Layer as _};

    let provider = if config.enabled {
        let mut exporter = opentelemetry_otlp::SpanExporter::builder().with_tonic();
        if let Some(endpoint) = &config.endpoint {
            exporter = exporter.with_endpoint(endpoint.clone());
        }
        let exporter = exporter.build()?;
        let resource = opentelemetry_sdk::Resource::builder()
            .with_service_name(config.service_name.clone())
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build();
        Some(
            opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_sampler(sampler(config))
                .with_resource(resource)
                .build(),
        )
    } else {
        None
    };

    let telemetry = provider.as_ref().map(|provider| {
        let scope = InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        tracing_opentelemetry::layer().with_tracer(provider.tracer_with_scope(scope))
    });
    let formatting = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_span_events(FmtSpan::CLOSE)
        .with_filter(EnvFilter::try_new(filter)?);

    if let Err(error) = tracing_subscriber::registry()
        .with(formatting)
        .with(telemetry)
        .try_init()
    {
        let mut guard = TelemetryGuard { provider };
        let _ = guard.shutdown_inner();
        return Err(Box::new(error));
    }

    Ok(TelemetryGuard { provider })
}

/// Install local formatted tracing without OpenTelemetry export.
///
/// Idempotent: a second call is a no-op, which is what lets tests and utility
/// binaries call it freely.
pub fn init_tracing(filter: &str) {
    let _ = init_tracing_with_telemetry(filter, &config::TelemetryConfig::default());
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_server::{
        TraceService, TraceServiceServer,
    };
    use opentelemetry_proto::tonic::collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Clone, Debug, Default)]
    struct RecordingExporter {
        spans: std::sync::Arc<std::sync::Mutex<Vec<SpanData>>>,
    }

    impl SpanExporter for RecordingExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let spans = self.spans.clone();
            async move {
                spans.lock().unwrap().extend(batch);
                Ok(())
            }
        }
    }

    fn exported_span_count(policy: config::TraceSampler, ratio: f64) -> usize {
        let exporter = RecordingExporter::default();
        let config = config::TelemetryConfig {
            sampler: policy,
            sample_ratio: ratio,
            ..config::TelemetryConfig::default()
        };
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .with_sampler(sampler(&config))
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("sampler-test")));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info_span!("sampled-operation").in_scope(|| {
                tracing::info!("span event");
            });
        });
        provider.force_flush().unwrap();
        let count = exporter.spans.lock().unwrap().len();
        provider.shutdown().unwrap();
        count
    }

    #[test]
    fn configured_sampler_controls_exported_traces() {
        use config::TraceSampler;

        assert_eq!(exported_span_count(TraceSampler::AlwaysOn, 1.0), 1);
        assert_eq!(exported_span_count(TraceSampler::AlwaysOff, 1.0), 0);
        assert_eq!(exported_span_count(TraceSampler::TraceIdRatio, 0.0), 0);
        assert_eq!(exported_span_count(TraceSampler::TraceIdRatio, 1.0), 1);
        assert_eq!(
            exported_span_count(TraceSampler::ParentBasedAlwaysOff, 1.0),
            0
        );
        assert_eq!(
            exported_span_count(TraceSampler::ParentBasedTraceIdRatio, 0.0),
            0
        );
        assert_eq!(
            exported_span_count(TraceSampler::ParentBasedTraceIdRatio, 1.0),
            1
        );
    }

    #[derive(Clone)]
    struct Collector {
        exports: tokio::sync::mpsc::UnboundedSender<Vec<String>>,
    }

    #[tonic::async_trait]
    impl TraceService for Collector {
        async fn export(
            &self,
            request: tonic::Request<ExportTraceServiceRequest>,
        ) -> Result<tonic::Response<ExportTraceServiceResponse>, tonic::Status> {
            let names = request
                .into_inner()
                .resource_spans
                .into_iter()
                .flat_map(|resource| resource.scope_spans)
                .flat_map(|scope| scope.spans)
                .map(|span| span.name)
                .collect();
            let _ = self.exports.send(names);
            Ok(tonic::Response::new(ExportTraceServiceResponse::default()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_flushes_a_tracing_span_to_an_otlp_collector() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (exports, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(TraceServiceServer::new(Collector { exports }))
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    async {
                        let _ = stopped.await;
                    },
                ),
        );

        let config = config::TelemetryConfig {
            enabled: true,
            endpoint: Some(endpoint),
            sampler: config::TraceSampler::AlwaysOn,
            ..config::TelemetryConfig::default()
        };
        let telemetry = init_tracing_with_telemetry("off", &config).unwrap();
        tracing::info_span!("otlp-export-test").in_scope(|| {
            tracing::info!("exported event");
        });
        telemetry.shutdown().unwrap();

        let names = tokio::time::timeout(std::time::Duration::from_secs(5), received.recv())
            .await
            .expect("the exporter timed out")
            .expect("the collector closed before receiving a request");
        assert!(
            names.iter().any(|name| name == "otlp-export-test"),
            "{names:?}"
        );

        let _ = stop.send(());
        serving.await.unwrap().unwrap();
    }
}
