//! Shared TCP/TLS and Unix-domain transport for administrative clients.

use std::path::{Path, PathBuf};

use hyper_util::rt::TokioIo;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tower::service_fn;

use crate::ctx;

/// One administrative client transport failure.
pub type ConnectError = Box<dyn std::error::Error + Send + Sync>;

/// Optional TLS material for a TCP control-plane connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientTls<'a> {
    /// PEM certificate authorities trusted for the server certificate.
    pub ca: Option<&'a Path>,
    /// PEM client certificate presented for mutual TLS.
    pub cert: Option<&'a Path>,
    /// PEM private key paired with `cert`.
    pub key: Option<&'a Path>,
    /// DNS name expected in the server certificate.
    pub server_name: Option<&'a str>,
}

impl ClientTls<'_> {
    fn configured(self) -> bool {
        self.ca.is_some() || self.cert.is_some() || self.key.is_some() || self.server_name.is_some()
    }
}

/// Connect to a shared control-plane TCP URI or a local `unix:///absolute/path`.
pub async fn connect(value: &str, tls: ClientTls<'_>) -> Result<Channel, ConnectError> {
    if let Some(raw_path) = value.strip_prefix("unix://") {
        if tls.configured() {
            return Err("TLS options cannot be used with a Unix-domain endpoint".into());
        }
        let path = PathBuf::from(raw_path);
        if !path.is_absolute() {
            return Err(format!("Unix-domain endpoint path must be absolute: '{raw_path}'").into());
        }
        let connector_path = path.clone();
        return Endpoint::from_static("http://localhost")
            .connect_with_connector(service_fn(move |_| {
                let path = connector_path.clone();
                async move {
                    tokio::net::UnixStream::connect(path)
                        .await
                        .map(TokioIo::new)
                }
            }))
            .await
            .map_err(|error| {
                format!(
                    "cannot connect to Unix-domain control endpoint '{}': {error}",
                    path.display()
                )
                .into()
            });
    }

    let mut endpoint = Endpoint::from_shared(value.to_owned())?;
    if tls.configured() {
        // The listener's twin, `yesno_server::tls::server_config`, installs the
        // provider here for the same reason: this crate's graph carries both
        // `ring` and the EC2 client's `aws-lc-rs`, so Rustls cannot select one
        // on its own and panics when a configuration is built. Installation is
        // idempotent and first-writer-wins, so a binary that already chose keeps
        // its choice.
        yesno_server::tls::install_crypto_provider();
        let mut config = ClientTlsConfig::new();
        if let Some(ca) = tls.ca {
            let pem = std::fs::read(ca).map_err(|e| ctx("read the TLS authorities", ca, e))?;
            config = config.ca_certificate(Certificate::from_pem(pem));
        }
        if let (Some(cert), Some(key)) = (tls.cert, tls.key) {
            config = config.identity(Identity::from_pem(
                std::fs::read(cert).map_err(|e| ctx("read the client certificate", cert, e))?,
                std::fs::read(key).map_err(|e| ctx("read the client private key", key, e))?,
            ));
        }
        if let Some(name) = tls.server_name {
            config = config.domain_name(name.to_owned());
        }
        endpoint = endpoint.tls_config(config)?;
    } else if value.starts_with("https://") {
        return Err("an https:// endpoint needs --ca so the server can be verified".into());
    }
    Ok(endpoint.connect().await?)
}

#[cfg(all(test, unix))]
mod tests {
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    use super::*;

    /// A mistyped `--ca` names the file it could not read.
    ///
    /// This is the most operator-visible `ENOENT` in the crate and it used to
    /// be a bare "No such file or directory ( os error 2 )", which says nothing
    /// an operator can act on. See
    /// `the-archive-sidecar-fails-with-enoent-under-load`.
    #[tokio::test]
    async fn an_unreadable_tls_authority_names_the_path() {
        let dir = tempdir().unwrap();
        let ca = dir.path().join("typo-in-the-ca-path.pem");
        let error = connect(
            "https://localhost:1",
            ClientTls {
                ca: Some(&ca),
                ..Default::default()
            },
        )
        .await
        .expect_err("a missing CA bundle cannot configure TLS");
        let message = error.to_string();
        assert!(
            message.contains(ca.to_str().unwrap()),
            "an unreadable CA must name the path, got {message:?}"
        );
    }

    #[tokio::test]
    async fn connects_to_an_absolute_unix_socket() {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let accepted = tokio::spawn(async move { listener.accept().await.unwrap() });

        let channel = connect(
            &format!("unix://{}", socket.display()),
            ClientTls::default(),
        )
        .await
        .unwrap();
        drop(channel);
        let _ = accepted.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_tls_on_a_unix_socket() {
        let error = connect(
            "unix:///run/yesno/control.sock",
            ClientTls {
                ca: Some(Path::new("ca.pem")),
                ..ClientTls::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("TLS options cannot"));
    }

    #[tokio::test]
    async fn rejects_a_relative_unix_socket() {
        let error = connect("unix://control.sock", ClientTls::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("must be absolute"));
    }
}
