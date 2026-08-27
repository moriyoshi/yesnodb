//! Turning `[server.flight.tls]` into a listener, and rotating it in place.
//!
//! # Backend
//!
//! `tls-ring`. The EC2 client's default HTTPS transport also compiles Rustls'
//! `aws-lc-rs` provider, so applications cannot rely on Rustls' single-feature
//! auto-selection. [`install_crypto_provider`] chooses `ring` before either
//! transport constructs a TLS configuration.
//!
//! # Hot reload
//!
//! Certificates are re-read on `SIGHUP`, so rotation does not need a restart.
//! [`ReloadableTls`] owns one immutable `Arc<ServerConfig>`; a reload builds a
//! **complete** replacement from the same configured paths and only then swaps
//! the pointer.
//!
//! **The swap is the last step, deliberately.** Reading a certificate,
//! parsing it, parsing the key, proving the two form an identity, and building
//! the client-auth verifier all happen against local variables. A reload that
//! fails at any of those steps returns an error having touched nothing, and the
//! listener keeps serving the material it already had. There is no state in
//! which the process holds a half-built configuration, and none in which it
//! holds no configuration at all.
//!
//! **In-flight handshakes see one generation and only one.** The accept loop
//! clones the `Arc` *before* it starts a handshake and hands that clone to the
//! `TlsAcceptor` for that one connection. The configuration behind an `Arc` is
//! never mutated, so a reload cannot be observed by a handshake already running:
//! it replaces the pointer that the *next* accept will read, and the old
//! configuration stays alive until the last handshake holding it finishes.
//! Established connections are untouched — a swap is not a disconnect.
//!
//! # Why the whole `ServerConfig`, and not `ResolvesServerCert`
//!
//! Rustls offers `ResolvesServerCert` for exactly this, and it was the obvious
//! design. It is not the one here, for two reasons:
//!
//! * It reloads **only the server's own certificate**. Mutual TLS is in use, and
//!   the client-auth trust roots ( `client_ca` ) live in the `ServerConfig`'s
//!   verifier, which no resolver can reach. An operator rotating a client CA —
//!   the same schedule, the same expiry — would find that half of the rotation
//!   silently needed a restart, which is the exact failure this module used to
//!   warn about.
//! * A resolver is called mid-handshake and rustls passes it no connection
//!   identity, so a resolver and a verifier reading the same swappable slot
//!   could disagree *within one handshake*. Snapshotting the whole
//!   configuration at accept time makes that unrepresentable.
//!
//! The cost is that a swap discards rustls' in-memory session-resumption cache,
//! so sessions established under the old configuration fall back to a full
//! handshake. That is a once-per-rotation cost measured in TLS handshakes.
//!
//! # Why this module terminates TLS itself
//!
//! `ServerTlsConfig` builds tonic's acceptor once, at `Server::builder()` time,
//! and keeps it private; there is no seam to swap. So the accept loop lives
//! here and hands tonic a stream of already-negotiated `TlsStream`s.
//! The stream item type is tonic's own — `tokio_rustls::server::TlsStream` —
//! which is what keeps `Request::peer_certs()` and `Request::remote_addr()`
//! working, and therefore keeps certificate-fingerprint principals working.
//! `TlsConnectInfo` has no public constructor, so an enum over "plain or TLS"
//! stream would silently lose the peer certificate; the two cases stay separate
//! for that reason.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};

use futures::Stream;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

use crate::config::{ConfigError, TlsConfig};

/// The ALPN protocol identifier gRPC needs. Tonic sets this on the configuration
/// it builds itself; a hand-built one must say so too or every client falls back
/// to HTTP/1 and fails.
const ALPN_H2: &[u8] = b"h2";

/// Select this crate's documented Rustls backend for the current process.
///
/// Installation is process-global and first-writer-wins. An embedding
/// application that selected a provider before calling yesno keeps its choice;
/// yesno's own binaries call this before initializing any transport.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn read(path: &Path, what: &str) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|e| {
        ConfigError::Invalid(format!(
            "cannot read the TLS {what} {}: {e}",
            path.display()
        ))
    })
}

/// Read and validate every file `cfg` names, or `None` for plaintext.
///
/// Everything that can fail happens here, before any caller has a pointer to
/// the result. That is what makes a failed reload harmless: the old
/// configuration is still installed because this returned `Err` before there was
/// anything to install.
///
/// `validate` has already refused the half-configured shapes — a cert with no
/// key, a `client_ca` with no identity — so this only has to read and parse.
fn build(cfg: &TlsConfig) -> Result<Option<ServerConfig>, ConfigError> {
    install_crypto_provider();
    let (Some(cert), Some(key)) = (&cfg.cert, &cfg.key) else {
        return Ok(None);
    };

    let chain = CertificateDer::pem_slice_iter(&read(cert, "certificate")?)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            ConfigError::Invalid(format!(
                "cannot parse the TLS certificate {}: {e}",
                cert.display()
            ))
        })?;
    if chain.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "the TLS certificate {} contains no certificate",
            cert.display()
        )));
    }
    let key_der = PrivateKeyDer::from_pem_slice(&read(key, "key")?).map_err(|e| {
        ConfigError::Invalid(format!("cannot parse the TLS key {}: {e}", key.display()))
    })?;

    let builder = ServerConfig::builder();
    let builder = match &cfg.client_ca {
        None => builder.with_no_client_auth(),
        Some(ca) => {
            let roots_pem = CertificateDer::pem_slice_iter(&read(ca, "client CA")?)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    ConfigError::Invalid(format!(
                        "cannot parse the TLS client CA {}: {e}",
                        ca.display()
                    ))
                })?;
            let mut roots = RootCertStore::empty();
            let (added, _ignored) = roots.add_parsable_certificates(roots_pem);
            // Zero usable roots would build a verifier that refuses every
            // client, which is an outage wearing a working configuration's
            // clothes. Refuse the reload instead.
            if added == 0 {
                return Err(ConfigError::Invalid(format!(
                    "the TLS client CA {} contains no usable certificate",
                    ca.display()
                )));
            }
            let verifier = WebPkiClientVerifier::builder(roots.into());
            // Inverted on purpose. Rustls asks to opt *in* to unauthenticated
            // clients; the configuration asks whether a certificate is
            // *required*, which is the question an operator is actually
            // answering. Optional is the useful default when some clients
            // authenticate with bearer tokens instead — the certificate then
            // identifies whoever has one, and the authorization layer decides
            // what an unidentified caller may do.
            let verifier = match cfg.require_client_auth {
                true => verifier,
                false => verifier.allow_unauthenticated(),
            };
            let verifier = verifier.build().map_err(|e| {
                ConfigError::Invalid(format!(
                    "the TLS client CA {} cannot be used to verify clients: {e}",
                    ca.display()
                ))
            })?;
            builder.with_client_cert_verifier(verifier)
        }
    };

    // The step that proves the key belongs to the certificate. A mismatched pair
    // parses cleanly and fails only here, which is why a reload must reach this
    // line before it is allowed to swap anything.
    let mut config = builder.with_single_cert(chain, key_der).map_err(|e| {
        ConfigError::Invalid(format!(
            "the TLS certificate {} and key {} do not form a usable identity: {e}",
            cert.display(),
            key.display()
        ))
    })?;
    config.alpn_protocols.push(ALPN_H2.to_vec());
    Ok(Some(config))
}

/// One listener's TLS material, replaceable without stopping the listener.
pub struct ReloadableTls {
    /// The paths a reload re-reads. Rotation replaces *file contents*; it never
    /// moves the configuration, so re-reading the same paths is the whole of it.
    source: TlsConfig,
    /// Which listener this is, for the log line an operator greps.
    scope: &'static str,
    /// The complete configuration, immutable, behind one pointer. Readers
    /// clone the `Arc` and never look again; a writer replaces the pointer. The
    /// lock is held only across the pointer move, never across a handshake.
    current: RwLock<Arc<ServerConfig>>,
    /// Bumped on every successful swap. Nothing depends on it; it is what a log
    /// line and a test can point at to say a rotation actually happened.
    generation: AtomicU64,
}

/// Every reloadable listener in this process, weakly.
///
/// SIGHUP is a process-level event and the set of listeners is a process-level
/// fact, so the registry is too. `Weak` means a listener that has stopped drops
/// out on the next pass rather than being reloaded forever.
static REGISTERED: Mutex<Vec<Weak<ReloadableTls>>> = Mutex::new(Vec::new());

/// What one [`reload_all`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReloadReport {
    /// Listeners now serving newly read material.
    pub reloaded: usize,
    /// Listeners that refused the new material and kept the old.
    pub failed: usize,
}

impl ReloadableTls {
    /// Read the configured material and, if there is any, hold it reloadable.
    ///
    /// `None` means plaintext, which is not an error and not reloadable.
    pub fn new(
        cfg: &TlsConfig,
        scope: &'static str,
    ) -> Result<Option<Arc<Self>>, Box<dyn std::error::Error + Send + Sync>> {
        let Some(config) = build(cfg)? else {
            return Ok(None);
        };
        let this = Arc::new(Self {
            source: cfg.clone(),
            scope,
            current: RwLock::new(Arc::new(config)),
            generation: AtomicU64::new(0),
        });
        REGISTERED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Arc::downgrade(&this));
        Ok(Some(this))
    }

    /// The configuration one connection will use, start to finish.
    pub(crate) fn snapshot(&self) -> Arc<ServerConfig> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Re-read the configured files and, only if all of them are good, install
    /// them for connections accepted from now on.
    ///
    /// On any error the listener is left exactly as it was. That is the
    /// property that matters: a rotation with a typo in it must be a logged
    /// failure, not an outage.
    pub fn reload(&self) -> Result<u64, ConfigError> {
        let Some(config) = build(&self.source)? else {
            // Unreachable through `new`, which only returns `Some` when the
            // paths are set, and the paths do not change while running.
            return Err(ConfigError::Invalid(
                "the TLS configuration no longer names a certificate".into(),
            ));
        };
        // The swap, and nothing else, under the lock.
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(config);
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        tracing::info!(
            scope = self.scope,
            generation,
            "TLS material reloaded; new connections use it, established ones are untouched"
        );
        Ok(generation)
    }
}

/// Reload every TLS listener in this process.
///
/// Each listener is independent: one that refuses its new material keeps the old
/// and does not stop the others from taking theirs.
pub fn reload_all() -> ReloadReport {
    let listeners: Vec<Arc<ReloadableTls>> = {
        let mut registered = REGISTERED.lock().unwrap_or_else(PoisonError::into_inner);
        registered.retain(|weak| weak.strong_count() > 0);
        registered.iter().filter_map(Weak::upgrade).collect()
    };
    let mut report = ReloadReport::default();
    for listener in listeners {
        match listener.reload() {
            Ok(_) => report.reloaded += 1,
            Err(error) => {
                report.failed += 1;
                // `error`, not `warn`. An operator who has just rotated a
                // certificate and whose server is still presenting the old one
                // has a deadline, and this is the line that tells them.
                tracing::error!(
                    scope = listener.scope,
                    error = %error,
                    "TLS reload refused; the listener keeps its previous certificate"
                );
            }
        }
    }
    if report.reloaded + report.failed == 0 {
        tracing::info!("TLS reload requested, but no listener is configured for TLS");
    }
    report
}

/// Reload on every SIGHUP, for as long as the process runs.
///
/// Recurring, where `yesnod`'s SIGUSR1 promotion is a one-shot future
/// selected in the role loop: rotation happens many times over a process' life
/// and must not end the loop it is watched from. The registration itself and the
/// log-and-carry-on when it fails follow that precedent exactly, and the handler
/// is installed *before* this function returns so a signal that arrives
/// immediately afterwards cannot land on the default disposition and kill the
/// process.
#[cfg(unix)]
pub fn watch_for_reload_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(hangup) => hangup,
        Err(error) => {
            tracing::error!(
                error = %error,
                "cannot listen for SIGHUP; TLS certificate reload is unavailable"
            );
            return;
        }
    };
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            let report = reload_all();
            tracing::info!(reloaded = report.reloaded, failed = report.failed, "SIGHUP");
        }
    });
}

#[cfg(not(unix))]
pub fn watch_for_reload_signal() {}

/// A TCP listener that negotiates TLS before tonic sees a byte.
///
/// Handshakes run concurrently in a `JoinSet` rather than inline, so one slow
/// client cannot stall the accept loop — the same shape tonic's own TLS path
/// uses, for the same reason. A handshake that fails is logged and dropped;
/// only accept errors reach tonic, which is where the decision to keep going or
/// stop already lives.
fn incoming_tls(
    listener: TcpListener,
    tls: Arc<ReloadableTls>,
) -> impl Stream<Item = io::Result<TlsStream<TcpStream>>> {
    struct State {
        listener: TcpListener,
        tls: Arc<ReloadableTls>,
        // `Box`ed, and not for tidiness: a negotiated `TlsStream` carries
        // Rustls' whole connection state, and an un-boxed one would make `Step`
        // below a kilobyte-plus enum that every loop iteration moves.
        handshakes: tokio::task::JoinSet<io::Result<Box<TlsStream<TcpStream>>>>,
    }

    enum Step {
        Accepted(io::Result<TcpStream>),
        Negotiated(Result<io::Result<Box<TlsStream<TcpStream>>>, tokio::task::JoinError>),
    }

    futures::stream::unfold(
        State {
            listener,
            tls,
            handshakes: tokio::task::JoinSet::new(),
        },
        |mut state| async move {
            loop {
                let State {
                    listener,
                    tls,
                    handshakes,
                } = &mut state;
                let step = if handshakes.is_empty() {
                    Step::Accepted(listener.accept().await.map(|(stream, _)| stream))
                } else {
                    tokio::select! {
                        accepted = listener.accept() => {
                            Step::Accepted(accepted.map(|(stream, _)| stream))
                        }
                        // `join_next` is cancel-safe, and the set cannot empty
                        // while this future holds it, so `Some` always matches.
                        Some(negotiated) = handshakes.join_next() => Step::Negotiated(negotiated),
                    }
                };
                match step {
                    Step::Accepted(Ok(stream)) => {
                        // **The snapshot point.** One `Arc` clone, taken
                        // before the handshake starts and used for all of it. A
                        // reload after this line changes what the *next*
                        // connection gets and cannot touch this one.
                        let config = tls.snapshot();
                        handshakes.spawn(async move {
                            TlsAcceptor::from(config).accept(stream).await.map(Box::new)
                        });
                    }
                    // Accept errors are tonic's to classify: it already
                    // distinguishes the transient ones from the fatal ones, and
                    // duplicating that judgement here would be a second policy.
                    Step::Accepted(Err(error)) => return Some((Err(error), state)),
                    Step::Negotiated(Ok(Ok(stream))) => return Some((Ok(*stream), state)),
                    Step::Negotiated(Ok(Err(error))) => {
                        tracing::debug!(error = %error, "TLS handshake failed");
                    }
                    Step::Negotiated(Err(error)) => {
                        tracing::debug!(error = %error, "a TLS handshake task did not finish");
                    }
                }
            }
        },
    )
}

/// Serve `router` on `listener`, over TLS when there is any, until `shutdown`.
///
/// The two arms are separate on purpose: the TLS one yields
/// `tokio_rustls::server::TlsStream`, which is the type tonic reads peer
/// certificates out of, and there is no third type that can stand for both
/// without losing them.
pub async fn serve_router<F>(
    router: tonic::transport::server::Router,
    listener: TcpListener,
    tls: Option<Arc<ReloadableTls>>,
    shutdown: F,
) -> Result<(), tonic::transport::Error>
where
    F: std::future::Future<Output = ()>,
{
    match tls {
        Some(tls) => {
            router
                .serve_with_incoming_shutdown(incoming_tls(listener, tls), shutdown)
                .await
        }
        None => {
            router
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(listener),
                    shutdown,
                )
                .await
        }
    }
}
