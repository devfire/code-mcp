# code-mcp

A streamable-HTTP MCP server that exposes fast filesystem search and read tools (`grep`, `find`, `cat`) to LLM clients.

The point: Claude Code's local MCP support is stdio-only. This server speaks streamable HTTP so a single instance running on a dev box can be reached over the LAN by Claude Code, Cursor, Zed, or any other MCP client that supports HTTP transport.

> [!WARNING]
> No authentication. **Run this on a private LAN only.** Anyone who can reach the bind address can use the tools. `--project` scopes what they can read.

## Tools

### `grep`
Regex search across files using parallel directory traversal (`ignore` + `grep-searcher`).

| arg                  | type            | default | notes                                                       |
| -------------------- | --------------- | ------- | ----------------------------------------------------------- |
| `directory`          | `string`        | —       | required                                                    |
| `pattern`            | `string`        | —       | required; Rust `regex` flavor — no lookaround/backrefs      |
| `before_context`     | `int`           | `0`     |                                                             |
| `after_context`      | `int`           | `0`     |                                                             |
| `max_results`        | `int`           | `100`   | exact cap (no over-shoot)                                   |
| `case_insensitive`   | `bool`          | `false` | equivalent to `(?i)` prefix in `pattern`                    |
| `include_hidden`     | `bool`          | `false` |                                                             |
| `follow_symlinks`    | `bool`          | `false` |                                                             |
| `respect_gitignore`  | `bool`          | `true`  |                                                             |
| `file_extensions`    | `string[]`      | `[]`    | e.g. `["rs", "toml"]`; empty = all                          |
| `max_bytes`          | `int`           | ~5 MiB  | hard cap on response size; appends `[truncated: byte cap]`  |

Walker errors and search errors are tallied and reported as a `[notice: N entry errors, M search errors; first: ...]` footer rather than silently dropped.

### `find`
Find files by regex.

| arg                 | type     | default | notes                                                |
| ------------------- | -------- | ------- | ---------------------------------------------------- |
| `directory`         | `string` | —       | required                                             |
| `pattern`           | `string` | —       | required                                             |
| `max_results`       | `int`    | `100`   |                                                      |
| `include_hidden`    | `bool`   | `false` |                                                      |
| `respect_gitignore` | `bool`   | `true`  |                                                      |
| `match_basename`    | `bool`   | `true`  | when `false`, the regex matches the full path        |

### `memories`
Load persisted context (conventions, project facts, prior feedback) for this server. Available only when `--memory-dir` was set at startup; otherwise returns an `invalid_params` error.

| arg    | type     | default | notes                                                                       |
| ------ | -------- | ------- | --------------------------------------------------------------------------- |
| `name` | `string` | —       | Optional filename within the memory dir (e.g. `user_role.md`). Plain basename only — `..`, `/`, `\` are rejected. |

Without `name`: returns the contents of `<memory-dir>/MEMORY.md` if present, otherwise a listing of `*.md` files in the dir. Re-reads on every call, so edits made on disk are picked up live.

The model is told about this tool via `InitializeResult.instructions` whenever the server is launched with `--memory-dir`. The expected pattern is:

1. On session start, the model calls `memories` with no args → gets the index.
2. The index points to specific memory files via `cat`-able paths or names.
3. The model loads what's relevant via `cat` (or another `memories(name=...)` call).

This mirrors Claude Code's auto-memory pattern.

### `cat`
Read file contents with pagination.

| arg         | type     | default | notes                                                                |
| ----------- | -------- | ------- | -------------------------------------------------------------------- |
| `file_path` | `string` | —       | required                                                             |
| `offset`    | `int`    | `0`     | 0-based line number to start from                                    |
| `max_lines` | `int`    | `2000`  | appends `[truncated: line cap]` if more lines remain                 |
| `max_bytes` | `int`    | ~5 MiB  | appends `[truncated: byte cap]` if hit mid-line (UTF-8-safe cut)     |

Use `offset` to page: if the response ends with `[truncated: line cap]`, call again with `offset = previous_offset + max_lines`.

## Build & run

```sh
cargo build --release
./target/release/code-mcp --bind 0.0.0.0:8080 --project ./my/repo
```

Flags:

- `--bind <addr:port>` — default `0.0.0.0:8080`.
- `--project <path>` — **required**. Every path the tools touch is canonicalized and required to lie within this directory; anything outside is rejected with `invalid_params`. Symlinks in input paths are resolved before the check, so `cat /proj/link-to-etc-passwd` is rejected because its canonical form is `/etc/passwd`. The server refuses to start without it.
- `--memory-dir <path>` — optional. If set, enables the `memories` tool and reads `<path>/instructions.md` (if present) into the `InitializeResult.instructions` payload sent to the model on connect.
- `--max-sessions <N>` — default `64`. Hard cap on concurrent stateful sessions in the rmcp `LocalSessionManager`. New initialize POSTs are rejected with `503 Service Unavailable` + `Retry-After: 5` once the cap is met. Existing-session traffic (any POST carrying `Mcp-Session-Id`) passes through untouched.
- `--initialize-rate-per-min <R>` — default `12`. Per-peer cap on **new** initialize requests, expressed as a per-minute token bucket (capacity = `R`, refilling continuously over 60 s). When exhausted, new initializes from that peer return `429 Too Many Requests` + `Retry-After: <secs>`. A misconfigured client that reconnects in a tight loop gets throttled here instead of pinning unbounded session state. Default `12`/min ≈ one fresh session every 5 s sustained — well above any healthy reconnect rate.
- `--trust-forwarded-for` — default `false`. When set, the gate uses the leftmost entry of `X-Forwarded-For` as the peer IP for rate-limiting. Only enable when the server sits behind a reverse proxy you control; the header is forgeable by any direct client.
- `--session-idle-timeout-secs <N>` — default `1800` (30 min). Idle timeout for stateful sessions. A background reaper task closes any session whose last observed request is older than this, so abandoned clients (process killed, network gone, no DELETE sent) don't pin slots against `--max-sessions` indefinitely. The cap defends against bursts; the reaper handles long-lived zombies.
- `--session-sweep-interval-secs <N>` — default `60`. How often the reaper sweeps for idle sessions.

### Scope semantics

With `--project ./my/repo` set:

- ✅ `cat ./my/repo/src/main.rs` — inside the root, allowed
- ✅ `grep ./my/repo --pattern foo` — directory inside the root, allowed
- ❌ `cat /etc/passwd` — outside the root, rejected
- ❌ `cat ./my/repo/../../etc/passwd` — canonicalizes to `/etc/passwd`, rejected
- ❌ `cat ./my/repo/link-to-secret` (symlink to `/etc/passwd`) — symlink resolves outside root, rejected

The `--memory-dir` is **not** required to be inside `--project` — it's server-side config, not user-driven file access.

### Memory dir layout

```
<memory-dir>/
├── instructions.md     # appended to InitializeResult.instructions on connect
├── MEMORY.md           # returned by memories() with no args
├── user_role.md        # returned by memories(name="user_role.md")
├── feedback_testing.md
└── project_status.md
```

The `instructions.md` file is read once at startup. The other files are read on demand by the `memories` and `cat` tools, so editing them does not require a restart.

Logging via `RUST_LOG`:

```sh
RUST_LOG=debug,rmcp=info ./target/release/code-mcp
```

Default level is `info,rmcp=info`. Press Ctrl-C for graceful shutdown (cancels the rmcp `CancellationToken`, drains active sessions).

## Connecting from Claude Code

Add to `~/.claude.json` (or use `claude mcp add`):

```json
{
  "mcpServers": {
    "code-mcp": {
      "type": "http",
      "url": "http://your-dev-box.lan:8080/"
    }
  }
}
```

Other MCP clients with streamable-HTTP support (Cursor, Zed, etc.) take similar config.

## Development

```sh
cargo test           # tools, scope, limiter, and gate-middleware tests
cargo clippy --all-targets -- -D warnings
```

## Notes & non-goals

- **No auth** — by design (LAN deployment). For path scoping, use `--project`.
- The `regex` crate has no lookaround or backreferences. Patterns that need them won't compile and you'll get an `invalid_params` MCP error.
- `.gitignore` is honored only inside a directory tree that contains a `.git/` directory (this is `ignore` crate behavior, not ours).
- Each parallel-walker worker keeps a thread-local `String` buffer and ships it to the main thread via `mpsc`; counter is `AtomicUsize` with `fetch_add`-based exact capping. There is no `Arc<Mutex<...>>` on the hot path.
