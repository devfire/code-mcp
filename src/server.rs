use std::fmt::Write;
use std::path::PathBuf;

use rmcp::{
    ServerHandler, handler::server::wrapper::Parameters, model::CallToolResult, tool, tool_handler,
    tool_router,
};

use crate::args::{CatArgs, FindArgs, GrepArgs, MemoriesArgs, StringOrVec};
use crate::error::{ToolResult, join_error, tool_error};
use crate::memory::load_memory;
use crate::scope::Scope;
use crate::tools::{self, OutputMode};

/// The MCP server handler. Owns the tool router, the optional memory dir,
/// the extra instructions loaded at startup, and the filesystem [`Scope`].
///
/// Constructed once per session by the `StreamableHttpService` closure in
/// `main` (so each session gets its own cheap `Clone` of the `Scope` and
/// config). All tool handlers run their blocking work in `spawn_blocking`
/// and delegate to [`crate::tools`].
#[derive(Clone)]
pub struct CodeMcpServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
    memory_dir: Option<PathBuf>,
    extra_instructions: Option<String>,
    scope: Scope,
}

impl CodeMcpServer {
    /// Construct a new server instance. `extra_instructions` is the contents
    /// of `<memory-dir>/instructions.md` (loaded once at startup) and is
    /// appended to the `InitializeResult.instructions` payload.
    pub fn new(
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
        description = "Regex search across files (parallel, gitignore-aware). output_mode: 'files_with_matches' (default — list matching file paths), 'content' (matching lines with line numbers), 'count' (per-file match tallies). Other options: case_insensitive, file_extensions, before/after_context, include_hidden, follow_symlinks, max_results, max_bytes."
    )]
    async fn grep(&self, Parameters(args): Parameters<GrepArgs>) -> ToolResult<CallToolResult> {
        let directory = match self.scope.check(&args.directory) {
            Ok(d) => d,
            Err(e) => return Ok(tool_error(e)),
        };
        let output_mode = match OutputMode::from_str_lossy(&args.output_mode) {
            Ok(m) => m,
            Err(e) => return Ok(tool_error(e)),
        };
        let res = tokio::task::spawn_blocking(move || {
            let opts = tools::GrepOptions {
                before_context: args.before_context,
                after_context: args.after_context,
                max_results: args.max_results,
                case_insensitive: args.case_insensitive,
                include_hidden: args.include_hidden,
                follow_symlinks: args.follow_symlinks,
                respect_gitignore: args.respect_gitignore,
                file_extensions: args
                    .file_extensions
                    .map(StringOrVec::into_vec)
                    .unwrap_or_default(),
                max_bytes: args.max_bytes,
                output_mode,
            };
            tools::grep(&directory.to_string_lossy(), &args.pattern, opts)
        })
        .await
        .map_err(join_error)?;

        match res {
            Ok(r) => Ok(r.into_call_tool_result()),
            Err(e) => Ok(tool_error(e)),
        }
    }

    #[tool(
        description = "Find files by regex (matches basename by default; set match_basename=false to match full path). Options: include_hidden, respect_gitignore, max_results."
    )]
    async fn find(&self, Parameters(args): Parameters<FindArgs>) -> ToolResult<CallToolResult> {
        let directory = match self.scope.check(&args.directory) {
            Ok(d) => d,
            Err(e) => return Ok(tool_error(e)),
        };
        let res = tokio::task::spawn_blocking(move || {
            let opts = tools::FindOptions {
                max_results: args.max_results,
                include_hidden: args.include_hidden,
                respect_gitignore: args.respect_gitignore,
                match_basename: args.match_basename,
            };
            tools::find(&directory.to_string_lossy(), &args.pattern, opts)
        })
        .await
        .map_err(join_error)?;

        match res {
            Ok(r) => Ok(r.into_call_tool_result()),
            Err(e) => Ok(tool_error(e)),
        }
    }

    #[tool(
        description = "Read file contents. Use offset to paginate long files; max_lines / max_bytes cap the response size."
    )]
    async fn cat(&self, Parameters(args): Parameters<CatArgs>) -> ToolResult<CallToolResult> {
        let file_path = match self.scope.check(&args.file_path) {
            Ok(p) => p,
            Err(e) => return Ok(tool_error(e)),
        };
        let res = tokio::task::spawn_blocking(move || {
            tools::cat(
                &file_path.to_string_lossy(),
                args.offset,
                args.max_lines,
                args.max_bytes,
            )
        })
        .await
        .map_err(join_error)?;

        match res {
            Ok(r) => Ok(r.into_call_tool_result()),
            Err(e) => Ok(tool_error(e)),
        }
    }

    #[tool(
        description = "Load persisted memories for this server. With no `name`, returns the index (MEMORY.md if present, otherwise a listing of *.md files in the memory dir). With `name`, returns the contents of that memory file. Errors if --memory-dir was not configured."
    )]
    async fn memories(
        &self,
        Parameters(args): Parameters<MemoriesArgs>,
    ) -> ToolResult<CallToolResult> {
        let dir = match self.memory_dir.clone() {
            Some(d) => d,
            None => {
                return Ok(CallToolResult {
                    content: vec![rmcp::model::Content::text(
                        "memory dir not configured; start server with --memory-dir <path>",
                    )],
                    structured_content: None,
                    is_error: Some(true),
                    meta: None,
                });
            }
        };

        let res = tokio::task::spawn_blocking(move || load_memory(&dir, args.name.as_deref()))
            .await
            .map_err(join_error)?;

        match res {
            Ok(content) => {
                let resp = tools::ToolResponse {
                    content,
                    truncated: false,
                    truncation_reason: None,
                    match_count: None,
                    entry_error_count: None,
                    search_error_count: None,
                    first_error: None,
                };
                Ok(resp.into_call_tool_result())
            }
            Err(e) => Ok(tool_error(e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for CodeMcpServer {
    fn get_info(&self) -> rmcp::model::InitializeResult {
        let mut instructions = String::from("code-mcp: filesystem search and read tools.\n\n");
        let _ = write!(
            instructions,
            "All paths are scoped to the project root: {}. \
Paths outside this directory (or symlinks resolving outside it) are rejected \
with `invalid_params`.\n\n",
            self.scope.root().display()
        );
        instructions.push_str(
            "\
Regex flavor: Rust `regex` crate. No lookaround or backreferences. \
Use the inline flag (?i) at the start of a pattern for case-insensitive matching, \
or pass case_insensitive: true to grep.

`grep` supports three output modes via the `output_mode` parameter:
- `files_with_matches` (default): returns only the paths of files containing at \
least one match. This is the most token-efficient mode for broad reconnaissance \
(\"which files mention X?\"). Use `cat` to read specific files afterwards.
- `content`: returns matching lines with line numbers (the traditional grep output).
- `count`: returns per-file match tallies as `path: N` lines.

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

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn grep_args_accepts_file_extensions_as_string() -> TestResult {
        let json = r#"{"directory":"/tmp","pattern":"x","file_extensions":"sql"}"#;
        let args: GrepArgs = serde_json::from_str(json)?;
        let v = args.file_extensions.unwrap().into_vec();
        assert_eq!(v, vec!["sql".to_string()]);
        Ok(())
    }

    #[test]
    fn grep_args_accepts_file_extensions_as_array() -> TestResult {
        let json = r#"{"directory":"/tmp","pattern":"x","file_extensions":["rs","toml"]}"#;
        let args: GrepArgs = serde_json::from_str(json)?;
        let v = args.file_extensions.unwrap().into_vec();
        assert_eq!(v, vec!["rs".to_string(), "toml".to_string()]);
        Ok(())
    }
}
