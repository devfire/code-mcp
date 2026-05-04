mod error;
mod gate;
mod limiter;
mod scope;
mod tools;

use scope::Scope;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rmcp::{
    ServerHandler,
    handler::server::wrapper::Parameters,
    schemars, tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::gate::{GateCtx, gate};
use crate::limiter::PeerLimiter;

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

    /// Trust the leftmost entry of `X-Forwarded-For` as the peer IP
    /// instead of the TCP socket address. Only set this if the server
    /// sits behind a reverse proxy that you control — `X-Forwarded-For`
    /// is forgeable by any direct client.
    #[arg(long, default_value_t = false)]
    trust_forwarded_for: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GrepArgs {
    #[schemars(description = "Directory to search in")]
    directory: String,
    #[schemars(description = "Regex pattern to search for (Rust regex; no lookaround/backrefs). Use (?i) for case-insensitive.")]
    pattern: String,
    #[schemars(description = "Number of lines of context before each match (default 0)")]
    before_context: Option<usize>,
    #[schemars(description = "Number of lines of context after each match (default 0)")]
    after_context: Option<usize>,
    #[schemars(description = "Maximum number of results to return")]
    max_results: Option<usize>,
    #[schemars(description = "Case-insensitive search (default false). Equivalent to prefixing pattern with (?i).")]
    case_insensitive: Option<bool>,
    #[schemars(description = "Include hidden files and directories (default false)")]
    include_hidden: Option<bool>,
    #[schemars(description = "Follow symbolic links (default false)")]
    follow_symlinks: Option<bool>,
    #[schemars(description = "Respect .gitignore files (default true)")]
    respect_gitignore: Option<bool>,
    #[schemars(description = "Restrict to files with these extensions (e.g. [\"rs\", \"toml\"]). Empty means all files.")]
    file_extensions: Option<Vec<String>>,
    #[schemars(description = "Hard cap on total response size in bytes (default ~5 MiB). Truncates with a marker.")]
    max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FindArgs {
    #[schemars(description = "Directory to search in")]
    directory: String,
    #[schemars(description = "Regex pattern to match against filenames (Rust regex; no lookaround/backrefs)")]
    pattern: String,
    #[schemars(description = "Maximum number of results to return")]
    max_results: Option<usize>,
    #[schemars(description = "Include hidden files and directories (default false)")]
    include_hidden: Option<bool>,
    #[schemars(description = "Respect .gitignore files (default true)")]
    respect_gitignore: Option<bool>,
    #[schemars(description = "Match the basename only (default true). Set false to match the full path.")]
    match_basename: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CatArgs {
    #[schemars(description = "Path to the file to read")]
    file_path: String,
    #[schemars(description = "Line offset to start from (0-based, default 0). Use to paginate long files.")]
    offset: Option<usize>,
    #[schemars(description = "Maximum number of lines to return")]
    max_lines: Option<usize>,
    #[schemars(description = "Maximum number of bytes to return")]
    max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct MemoriesArgs {
    #[schemars(description = "Optional memory file name (relative to memory dir, e.g. \"user_role.md\"). If omitted, returns the index from MEMORY.md or a directory listing.")]
    name: Option<String>,
}

#[derive(Clone)]
struct CodeMcpServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    memory_dir: Option<PathBuf>,
    extra_instructions: Option<String>,
    scope: Scope,
}

impl CodeMcpServer {
    fn new(
        memory_dir: Option<PathBuf>,
        extra_instructions: Option<String>,
        scope: Scope,
    ) -> Self {
        Self {
            tool_router: Self::tool_router(),
            memory_dir,
            extra_instructions,
            scope,
        }
    }
}

#[tool_router]
impl CodeMcpServer {
    #[tool(
        description = "Regex search across files (parallel, gitignore-aware). Options: case_insensitive, file_extensions, before/after_context, include_hidden, follow_symlinks, max_results, max_bytes."
    )]
    async fn grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<String, rmcp::ErrorData> {
        let directory = self.scope.check(&args.directory)?;
        let res = tokio::task::spawn_blocking(move || {
            let opts = tools::GrepOptions {
                before_context: args.before_context.unwrap_or(0),
                after_context: args.after_context.unwrap_or(0),
                max_results: args.max_results,
                case_insensitive: args.case_insensitive.unwrap_or(false),
                include_hidden: args.include_hidden.unwrap_or(false),
                follow_symlinks: args.follow_symlinks.unwrap_or(false),
                respect_gitignore: args.respect_gitignore.unwrap_or(true),
                file_extensions: args.file_extensions.unwrap_or_default(),
                max_bytes: args.max_bytes,
            };
            tools::grep(&directory.to_string_lossy(), &args.pattern, opts)
        })
        .await
        .map_err(|e| {
            rmcp::ErrorData::internal_error(
                "internal_error",
                Some(serde_json::json!({"error": e.to_string()})),
            )
        })??;

        Ok(res)
    }

    #[tool(
        description = "Find files by regex (matches basename by default; set match_basename=false to match full path). Options: include_hidden, respect_gitignore, max_results."
    )]
    async fn find(
        &self,
        Parameters(args): Parameters<FindArgs>,
    ) -> Result<String, rmcp::ErrorData> {
        let directory = self.scope.check(&args.directory)?;
        let res = tokio::task::spawn_blocking(move || {
            let opts = tools::FindOptions {
                max_results: args.max_results,
                include_hidden: args.include_hidden.unwrap_or(false),
                respect_gitignore: args.respect_gitignore.unwrap_or(true),
                match_basename: args.match_basename.unwrap_or(true),
            };
            tools::find(&directory.to_string_lossy(), &args.pattern, opts)
        })
        .await
        .map_err(|e| {
            rmcp::ErrorData::internal_error(
                "internal_error",
                Some(serde_json::json!({"error": e.to_string()})),
            )
        })??;

        Ok(res)
    }

    #[tool(
        description = "Read file contents. Use offset to paginate long files; max_lines / max_bytes cap the response size."
    )]
    async fn cat(&self, Parameters(args): Parameters<CatArgs>) -> Result<String, rmcp::ErrorData> {
        let file_path = self.scope.check(&args.file_path)?;
        let res = tokio::task::spawn_blocking(move || {
            tools::cat(
                &file_path.to_string_lossy(),
                args.offset.unwrap_or(0),
                args.max_lines,
                args.max_bytes,
            )
        })
        .await
        .map_err(|e| {
            rmcp::ErrorData::internal_error(
                "internal_error",
                Some(serde_json::json!({"error": e.to_string()})),
            )
        })??;

        Ok(res)
    }

    #[tool(
        description = "Load persisted memories for this server. With no `name`, returns the index (MEMORY.md if present, otherwise a listing of *.md files in the memory dir). With `name`, returns the contents of that memory file. Errors if --memory-dir was not configured."
    )]
    async fn memories(
        &self,
        Parameters(args): Parameters<MemoriesArgs>,
    ) -> Result<String, rmcp::ErrorData> {
        let dir = self.memory_dir.clone().ok_or_else(|| {
            rmcp::ErrorData::invalid_params(
                "invalid_request",
                Some(serde_json::json!({
                    "error": "memory dir not configured; start server with --memory-dir <path>"
                })),
            )
        })?;

        let res = tokio::task::spawn_blocking(move || load_memory(&dir, args.name.as_deref()))
            .await
            .map_err(|e| {
                rmcp::ErrorData::internal_error(
                    "internal_error",
                    Some(serde_json::json!({"error": e.to_string()})),
                )
            })??;

        Ok(res)
    }
}

fn load_memory(dir: &std::path::Path, name: Option<&str>) -> Result<String, error::AppError> {
    if !dir.is_dir() {
        return Err(error::AppError::NotFound(format!(
            "memory dir does not exist: {}",
            dir.display()
        )));
    }

    if let Some(name) = name {
        // Reject path traversal: name must be a single, non-empty path component.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(error::AppError::InvalidRequest(format!(
                "memory name must be a plain filename, got: {:?}",
                name
            )));
        }
        let path = dir.join(name);
        if !path.is_file() {
            return Err(error::AppError::NotFound(format!(
                "memory not found: {}",
                name
            )));
        }
        return Ok(std::fs::read_to_string(&path)?);
    }

    // No name: prefer MEMORY.md, otherwise list *.md files.
    let index = dir.join("MEMORY.md");
    if index.is_file() {
        return Ok(std::fs::read_to_string(&index)?);
    }

    let mut listing = String::from("# Memory dir contents\n\n");
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == "md")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    if entries.is_empty() {
        listing.push_str("(no .md files found; configure MEMORY.md or add memory files)\n");
    } else {
        for e in entries {
            if let Some(name) = e.file_name().to_str() {
                listing.push_str(&format!("- {}\n", name));
            }
        }
        listing.push_str(
            "\nUse `memories(name=\"...\")` to load a specific memory, \
or create a `MEMORY.md` index at the top level.\n",
        );
    }
    Ok(listing)
}

#[tool_handler]
impl ServerHandler for CodeMcpServer {
    fn get_info(&self) -> rmcp::model::InitializeResult {
        let mut instructions = String::from(
            "code-mcp: filesystem search and read tools.\n\n",
        );
        instructions.push_str(&format!(
            "All paths are scoped to the project root: {}. \
Paths outside this directory (or symlinks resolving outside it) are rejected \
with `invalid_params`.\n\n",
            self.scope.root().display()
        ));
        instructions.push_str(
            "\
Regex flavor: Rust `regex` crate. No lookaround or backreferences. \
Use the inline flag (?i) at the start of a pattern for case-insensitive matching, \
or pass case_insensitive: true to grep.

`find` matches the basename of each path by default. Set match_basename: false to \
match against the full path instead.

`.gitignore` files are respected by default for both grep and find. \
Set respect_gitignore: false to walk the entire tree, including ignored paths.

`cat` supports an `offset` (0-based line number) for paginating long files, plus \
optional `max_lines` and `max_bytes` caps.",
        );

        if self.memory_dir.is_some() {
            instructions.push_str(
                "\n\nThis server has a memory directory configured. \
Call the `memories` tool at the start of a session to load persisted context \
(conventions, project facts, prior feedback). \
With no arguments it returns an index; pass `name` to load a specific memory file. \
Individual memory files referenced in the index can also be read with `cat`.",
            );
        }

        if let Some(extra) = &self.extra_instructions {
            instructions.push_str("\n\n--- project instructions ---\n\n");
            instructions.push_str(extra);
        }

        rmcp::model::InitializeResult {
            protocol_version: rmcp::model::ProtocolVersion::V_2025_06_18,
            server_info: rmcp::model::Implementation {
                name: "code-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            capabilities: rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
            instructions: Some(instructions),
        }
    }
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
    });
    tracing::info!(
        max_sessions = args.max_sessions,
        initialize_rate_per_min = args.initialize_rate_per_min,
        trust_forwarded_for = args.trust_forwarded_for,
        "gate configured"
    );

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn load_memory_returns_index_when_memory_md_present() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("MEMORY.md"), "# index\n- foo\n")?;
        fs::write(td.path().join("foo.md"), "ignored\n")?;

        let out = load_memory(td.path(), None)?;
        assert!(out.starts_with("# index"), "got {:?}", out);
        Ok(())
    }

    #[test]
    fn load_memory_lists_md_files_when_no_index() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("a.md"), "")?;
        fs::write(td.path().join("b.md"), "")?;
        fs::write(td.path().join("ignore.txt"), "")?;

        let out = load_memory(td.path(), None)?;
        assert!(out.contains("- a.md"), "got {}", out);
        assert!(out.contains("- b.md"), "got {}", out);
        assert!(!out.contains("ignore.txt"), "got {}", out);
        Ok(())
    }

    #[test]
    fn load_memory_returns_named_file() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("user_role.md"), "data scientist\n")?;

        let out = load_memory(td.path(), Some("user_role.md"))?;
        assert_eq!(out, "data scientist\n");
        Ok(())
    }

    #[test]
    fn load_memory_rejects_path_traversal() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("ok.md"), "ok\n")?;

        for bad in ["../etc/passwd", "sub/foo.md", "..\\foo", "..", ""] {
            match load_memory(td.path(), Some(bad)) {
                Err(error::AppError::InvalidRequest(_)) => {}
                Err(error::AppError::NotFound(_)) if bad.is_empty() => {}
                other => {
                    return Err(format!("expected rejection for {:?}, got {:?}", bad, other).into());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn load_memory_errors_on_missing_dir() -> TestResult {
        let td = TempDir::new()?;
        let missing = td.path().join("nope");
        match load_memory(&missing, None) {
            Err(error::AppError::NotFound(_)) => Ok(()),
            other => Err(format!("expected NotFound, got {:?}", other).into()),
        }
    }
}
