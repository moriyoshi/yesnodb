use std::process::ExitCode;

use kube::Client;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("crd") {
        match yesno_operator::crd_yaml() {
            Ok(yaml) => {
                print!("{yaml}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("yesno-operator: cannot render CRD: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Before the Kubernetes client, which is the first thing here to build a
    // Rustls configuration. `yesno-server` brings the EC2 client, whose default
    // HTTPS transport compiles Rustls' `aws-lc-rs` provider, while `kube` and
    // `tonic` bring `ring` — so Rustls sees two providers, refuses to guess, and
    // panics on the first configuration built. Removing this line does not
    // fail a build or a unit test; it crash-loops the operator pod at startup.
    yesno_server::tls::install_crypto_provider();

    let client = match Client::try_default().await {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "cannot create Kubernetes client");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("starting yesno-operator");
    yesno_operator::run(client).await;
    ExitCode::SUCCESS
}
