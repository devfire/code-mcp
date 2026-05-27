mod args;
mod error;
mod gate;
mod limiter;
mod memory;
mod reaper;
mod scope;
mod server;
mod tools;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService,
    session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::gate::{GateCtx, gate};
use crate::limiter::PeerLimiter;
use crate::scope::Scope;
use crate::server::CodeMcpServer;

#[derive(Debug, Parser)]
#[command(name = "code-mcp", about = "Streamable HTTP MCP server for code search/read tools")]
struct Args {
    /// Address to bind, e.g. 0.0.0.0:8080
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: SocketAddr,

    /// Optional directory of persisted memories. If set:
    ///   * `<dir>/instructions.md` (if present) is appended to the MCP
    ///     `InitializeResult.instructions` payload sent on connect.
    ///   * The `memories` tool reads from this directory.
    #[arg(long)]
    memory_dir: Option<PathBuf>,

    /// Required project root. Every path the tools touch (grep/find
    /// directory, cat file_path) is canonicalized and must be within
    /// this directory; anything outside is rejected. Symlinks in input
    /// paths are resolved before the check, so a symlink pointing out
    /// of the project is also rejected.
    #[arg(long, required = true)]
    project: PathBuf,

    /// Hard cap on concurrent stateful sessions. Once reached, new
    /// initialize POSTs are rejected with 503 + Retry-After until at
    /// least one session closes. Existing-session traffic is unaffected.
    #[arg(long, default_value_t = 64)]
    max_sessions: usize,

    /// Per-peer cap on **new** initialize requests, expressed as a
    /// per-minute rate (token bucket of capacity = rate, refilling over
    /// 60s). When exhausted, new initializes from that peer get 429 +
    /// Retry-After. Existing-session traffic is unaffected.
    #[arg(long, default_value_t = 12)]
    initialize_rate_per_min: u32,

    /// Trust the rightmost entry of `X-Forwarded-For` as the peer IP
    /// instead of the TCP socket address. Assumes a single trusted proxy
    /// hop (e.g. AWS ALB) that appends the real client IP. Only set this
    /// if the server sits behind a reverse proxy that you control —
    /// entries to the left of the last hop are client-supplied and forgeable.
    #[arg(long, default_value_t = false)]
    trust_forwarded_for: bool,

    /// Idle timeout (seconds) for stateful sessions. Sessions whose last
    /// observed request is older than this are closed by the reaper, so
    /// abandoned clients (process killed, network gone, no DELETE sent)
    /// don't pin slots against `--max-sessions` indefinitely.
    #[arg(long, default_value_t = 1800)]
    session_idle_timeout_secs: u64,

    /// How often (seconds) the reaper sweeps for idle sessions.
    #[arg(long, default_value_t = 60)]
    session_sweep_interval_secs: u64,
}

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
