//! edge-control — entrypoint for the Edge_Server.
//!
//! Phase 1 minimal: bind axum HTTP on :8443 with stub routes.
//! mTLS + real handlers come in subsequent phases (see spec design 15.7).

use std::sync::Arc;
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod auth;
mod config;
mod routes;
mod state;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,edge_control=debug")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    let cfg = config::EdgeConfig::from_env()?;
    tracing::info!(region = %cfg.region, "edge-control starting");

    let app_state = Arc::new(state::AppState::initialize(&cfg).await?);
    let app = routes::build_router(app_state);

    let bind_addr = ("0.0.0.0", cfg.bind_port);
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!(addr = ?listener.local_addr()?, "edge-control listening");

    axum::serve(listener, app).await?;
    Ok(())
}
