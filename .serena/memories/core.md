# Project Core: code-mcp

MCP (Model Context Protocol) server exposing code-navigation tools (grep, find, cat) and memory management over an HTTP transport. Written in Rust.

## Source Map (`src/`)

- `main.rs` — entry point; wires up modules
- `cli.rs` — `Args` struct (clap-based CLI)
- `args.rs` — tool argument structs: `GrepArgs`, `FindArgs`, `CatArgs`, `MemoriesArgs`; serde default helpers
- `server.rs` — `CodeMcpServer` implements `ServerHandler`; registers tools
- `tools.rs` — core logic: `grep`, `find`, `cat`; `ToolResponse`, `GrepOptions`, `FindOptions`, `MatchSink`
- `scope.rs` — `Scope` enforces allowed root paths
- `gate.rs` — HTTP middleware; session ID extraction, peer IP
- `limiter.rs` — rate limiting
- `reaper.rs` — process/connection reaper
- `memory.rs` — in-process memory store for the memory tool
- `error.rs` — `AppError` enum + `From<AppError> for ErrorData`

## Key Invariants

- `grep` uses `grep-searcher` + `grep-regex` crates (NOT ripgrep binary or `rg` shell command)
- `ignore` crate handles gitignore/hidden-file filtering
- `ToolResponse` is the universal return type from all tools; carries truncation metadata
- Tool args separated from tool logic: `args.rs` ↔ `tools.rs`

See `mem:tech_stack`, `mem:conventions`, `mem:suggested_commands`, `mem:task_completion`.
