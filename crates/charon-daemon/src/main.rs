use anyhow::Result;
use charon_daemon::DaemonConfig;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let config = DaemonConfig::from_env()?;

    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("ctrl-c received, shutting down");
            signal_token.cancel();
        }
    });

    charon_daemon::serve(config, Some(shutdown)).await
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,charon_daemon=debug,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
