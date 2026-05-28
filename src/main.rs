mod args;
mod error;
mod gate;
mod limiter;
mod memory;
mod reaper;
mod scope;
mod server;
mod tools;
mod cli;

use std::sync::Arc;
use std::time::Duration;
use clap::Parser;

use std::net::SocketAddr;


use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::gate::{GateCtx, gate};
use crate::limiter::PeerLimiter;
use crate::scope::Scope;
use crate::server::CodeMcpServer;
use crate::cli::Args;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,rmcp=info".into()),
        )
        .init();

    let args = Args::parse();

    // If a memory dir is configured, load <dir>/instructions.md once at startup.
    // It's appended to the InitializeResult.instructions payload.
    let extra_instructions = if let Some(dir) = args.memory_dir.as_ref() {
        let path = dir.join("instructions.md");
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => {
                tracing::info!(path = %path.display(), "loaded extra instructions");
                Some(s)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read instructions.md");
                None
            }
        }
    } else {
        None
    };

    let scope = Scope::new(args.project.clone())?;
    tracing::info!(root = %scope.root().display(), "project scope active");

    let memory_dir = args.memory_dir.clone();
    let cancel = CancellationToken::new();
    let session_manager = Arc::new(LocalSessionManager::default());
    let sessions_for_gate = session_manager.clone();
    let sessions_for_reaper = session_manager.clone();
    let activity = Arc::new(reaper::ActivityTracker::new());
    let activity_for_reaper = activity.clone();
    let config = StreamableHttpServerConfig {
        sse_keep_alive: Some(Duration::from_secs(15)),
        stateful_mode: true,
        cancellation_token: cancel.clone(),
        // rmcp 0.16's StreamableHttpServerConfig does NOT include host-allowlist
        // or json_response fields, so there's nothing to disable for LAN access.
        // Keep `..Default::default()` for forward compatibility with future fields.
        ..Default::default()
    };
    let service = StreamableHttpService::new(
        move || {
            Ok(CodeMcpServer::new(
                memory_dir.clone(),
                extra_instructions.clone(),
                scope.clone(),
            ))
        },
        session_manager,
        config,
    );

    let gate_ctx = Arc::new(GateCtx {
        sessions: sessions_for_gate,
        max_sessions: args.max_sessions,
        limiter: PeerLimiter::per_minute(args.initialize_rate_per_min),
        trust_forwarded_for: args.trust_forwarded_for,
        activity,
    });
    tracing::info!(
        max_sessions = args.max_sessions,
        initialize_rate_per_min = args.initialize_rate_per_min,
        trust_forwarded_for = args.trust_forwarded_for,
        session_idle_timeout_secs = args.session_idle_timeout_secs,
        session_sweep_interval_secs = args.session_sweep_interval_secs,
        "gate configured"
    );

    tokio::spawn(reaper::reap_loop(
        sessions_for_reaper,
        activity_for_reaper,
        Duration::from_secs(args.session_idle_timeout_secs),
        Duration::from_secs(args.session_sweep_interval_secs),
        cancel.clone(),
    ));

    // Wrap the tower service in an axum Router and gate new-session POSTs
    // ahead of it, so misbehaving clients can't pin unbounded session state.
    let app = axum::Router::new()
        .fallback_service(service)
        .layer(axum::middleware::from_fn_with_state(gate_ctx, gate));

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!(addr = %args.bind, "listening");

    let shutdown = {
        let cancel = cancel.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
            cancel.cancel();
        }
    };

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;

    Ok(())
}
