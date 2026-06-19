use std::path::PathBuf;
use clap::Parser;
use std::net::SocketAddr;

/// Command-line arguments for `code-mcp`.
///
/// Parsed via clap. All fields are `pub(crate)` because they're only consumed
/// by `main`; the struct itself is `pub` so it can be referenced from other
/// crate modules.
#[derive(Debug, Parser)]
#[command(name = "code-mcp", about = "Streamable HTTP MCP server for code search/read tools")]
pub struct Args {
    /// Address to bind, e.g. 0.0.0.0:8080
    #[arg(long, default_value = "0.0.0.0:8080")]
    pub(crate) bind: SocketAddr,

    /// Optional directory of persisted memories. If set:
    ///   * `<dir>/instructions.md` (if present) is appended to the MCP
    ///     `InitializeResult.instructions` payload sent on connect.
    ///   * The `memories` tool reads from this directory.
    #[arg(long)]
    pub(crate) memory_dir: Option<PathBuf>,

    /// Required project root. Every path the tools touch (grep/find
    /// directory, cat `file_path`) is canonicalized and must be within
    /// this directory; anything outside is rejected. Symlinks in input
    /// paths are resolved before the check, so a symlink pointing out
    /// of the project is also rejected.
    #[arg(long, required = true)]
    pub(crate) project: PathBuf,

    /// Hard cap on concurrent stateful sessions. Once reached, new
    /// initialize POSTs are rejected with 503 + Retry-After until at
    /// least one session closes. Existing-session traffic is unaffected.
    #[arg(long, default_value_t = 64)]
    pub(crate) max_sessions: usize,

    /// Per-peer cap on **new** initialize requests, expressed as a
    /// per-minute rate (token bucket of capacity = rate, refilling over
    /// 60s). When exhausted, new initializes from that peer get 429 +
    /// Retry-After. Existing-session traffic is unaffected.
    #[arg(long, default_value_t = 12)]
    pub(crate) initialize_rate_per_min: u32,

    /// Trust the rightmost entry of `X-Forwarded-For` as the peer IP
    /// instead of the TCP socket address. Assumes a single trusted proxy
    /// hop (e.g. AWS ALB) that appends the real client IP. Only set this
    /// if the server sits behind a reverse proxy that you control —
    /// entries to the left of the last hop are client-supplied and forgeable.
    #[arg(long, default_value_t = false)]
    pub(crate) trust_forwarded_for: bool,

    /// Idle timeout (seconds) for stateful sessions. Sessions whose last
    /// observed request is older than this are closed by the reaper, so
    /// abandoned clients (process killed, network gone, no DELETE sent)
    /// don't pin slots against `--max-sessions` indefinitely.
    #[arg(long, default_value_t = 1800)]
    pub(crate) session_idle_timeout_secs: u64,

    /// How often (seconds) the reaper sweeps for idle sessions.
    #[arg(long, default_value_t = 60)]
    pub(crate) session_sweep_interval_secs: u64,
}
