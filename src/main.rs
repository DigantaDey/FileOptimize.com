//! Binary entry point: initialise logging, build state, start the cleaner, and serve.

use std::net::SocketAddr;

use fileoptimize::{cleanup, config::Config, create_app, state::AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    let tmp_dir = config.tmp_dir.clone();
    if let Err(e) = tokio::fs::create_dir_all(&tmp_dir).await {
        eprintln!("fatal: could not create tmp dir {}: {e}", tmp_dir.display());
        std::process::exit(1);
    }

    let state = AppState::new(config.clone());
    cleanup::spawn(state.clone());

    let app = create_app(state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .unwrap_or_else(|_| panic!("invalid HOST/PORT: {}:{}", config.host, config.port));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fatal: could not bind {addr}: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        "FileOptimize {} listening on http://{addr}",
        env!("CARGO_PKG_VERSION")
    );
    tracing::info!(tmp_dir = %tmp_dir.display(), "serving from temp dir");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

/// Resolves on SIGINT (Ctrl-C) or SIGTERM, enabling graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining connections");
}
