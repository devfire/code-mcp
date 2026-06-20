# code-mcp

A streamable-HTTP code intelligence MCP server for LLM clients.

The point: offer a network-based code insights for both humans & machines. Although probably more suitable for humans (talk to your code) rather than specific compiler-based code insights like those of Serena. 

Able to handle very large codebases. 

Re-uses ripgrep crates just like ripgrep itself.

> [!WARNING]
> Authentication & Authorization are outside the scope of this project.
>
> **Run this on a private LAN only.**
>
> Anyone who can reach the bind address can use the tools but `--project` scopes what they can read.
>
> Run this as docker OR with `chroot` for extra jailing.
> 
> NOTE: https://modelcontextprotocol.io/docs/tutorials/security/authorization is what you should be using regardless.

## Installation

### Pre-built binaries
Download the latest release from the [Releases page](https://github.com/devfire/code-mcp/releases).

### Docker (GHCR)

The included `Dockerfile` uses a multi-stage build with `cargo-chef` for optimal layer caching. The final image is based on `debian:bookworm-slim` (~80 MB) and contains only the stripped binary and `git` (needed by the `ignore` crate for `.gitignore` traversal).

Multi-arch images (`linux/amd64`, `linux/arm64`) are published to GHCR automatically on every release. Pull the pre-built image — no local build needed:

```sh
docker pull ghcr.io/devfire/code-mcp:latest
```

```sh
# Run with defaults (bind 0.0.0.0:8080, project /project)
docker run -p 8080:8080 -v /path/to/repo:/project:ro ghcr.io/devfire/code-mcp:latest
```

```sh
# Override any flag — all CLI args are supported natively via ENTRYPOINT
docker run -p 9090:9090 \
  -v /path/to/repo:/project:ro \
  -v /path/to/memories:/memories:ro \
  ghcr.io/devfire/code-mcp:latest \
  --bind 0.0.0.0:9090 \
  --project /project \
  --memory-dir /memories \
  --max-sessions 128 \
  --initialize-rate-per-min 20 \
  --session-idle-timeout-secs 3600
```

```sh
# With debug logging
docker run -p 8080:8080 -e RUST_LOG=debug,rmcp=info \
  -v /path/to/repo:/project:ro ghcr.io/devfire/code-mcp:latest
```

The `ENTRYPOINT` is the binary itself, so any arguments after the image name replace the default `CMD` and go directly to clap. 

Pass `--help` to see all options:

```sh
docker run --rm ghcr.io/devfire/code-mcp:latest --help
```

<details>
<summary>Build locally instead</summary>

If you want to build from source (e.g. for an unreleased commit or a fork):

```sh
docker build -t code-mcp .
docker run -p 8080:8080 -v /path/to/repo:/project:ro code-mcp
```

</details>

Flags:
<details>

- `--bind <addr:port>` — default `0.0.0.0:8080`.
- `--project <path>` — **required**. Every path the tools touch is canonicalized and required to lie within this directory; anything outside is rejected with `invalid_params`. Symlinks in input paths are resolved before the check, so `cat /proj/link-to-etc-passwd` is rejected because its canonical form is `/etc/passwd`. The server refuses to start without it.
- `--memory-dir <path>` — optional. If set, enables the `memories` tool and reads `<path>/instructions.md` (if present) into the `InitializeResult.instructions` payload sent to the model on connect.
- `--max-sessions <N>` — default `64`. Hard cap on concurrent stateful sessions in the rmcp `LocalSessionManager`. New initialize POSTs are rejected with `503 Service Unavailable` + `Retry-After: 5` once the cap is met. Existing-session traffic (any POST carrying `Mcp-Session-Id`) passes through untouched.
- `--initialize-rate-per-min <R>` — default `12`. Per-peer cap on **new** initialize requests, expressed as a per-minute token bucket (capacity = `R`, refilling continuously over 60 s). When exhausted, new initializes from that peer return `429 Too Many Requests` + `Retry-After: <secs>`. A misconfigured client that reconnects in a tight loop gets throttled here instead of pinning unbounded session state. Default `12`/min ≈ one fresh session every 5 s sustained — well above any healthy reconnect rate.
- `--trust-forwarded-for` — default `false`. When set, the gate uses the rightmost entry of `X-Forwarded-For` as the peer IP for rate-limiting. This assumes a single trusted proxy hop (e.g. AWS ALB) that appends the real client IP; entries to the left of the last hop are client-supplied and forgeable. Only enable when the server sits behind a reverse proxy you control.
- `--session-idle-timeout-secs <N>` — default `1800` (30 min). Idle timeout for stateful sessions. A background reaper task closes any session whose last observed request is older than this, so abandoned clients (process killed, network gone, no DELETE sent) don't pin slots against `--max-sessions` indefinitely. The cap defends against bursts; the reaper handles long-lived zombies.
- `--session-sweep-interval-secs <N>` — default `60`. How often the reaper sweeps for idle sessions.

</details>

### From source

```bash
cargo install --git https://github.com/devfire/code-mcp.git
```
## FAQ
Q: You think you can vibe code some nonsense, yolo it into Github and somehow that makes you an expert on code intelligence?

A: This was built almost entirely by CC, so what, it's 2026, get with the times. However, unlike a purely vibe coded "nonsense" (i.e. look, ma - I did a chat app on localhost RIP slack) I happen to know Rust well, so everything here has been human validated and approved accordingly.

Q: WTF is this - ever heard of Serena MCP?

A: Yes but this is very different. Serena uses the power of language servers & therefore compilers to answer questions. It is incredibly powerful but also really slow and can handle a reasonable number of projects at a time. Serena struggles with large mono repos with 100s of sub repos. This goes through 100s of repos like knife through butter.

Q: How can you get any intelligence out of `cat`, `find`, and `grep` - you mad?

A: You'd be surprised! Modern SOTA models (Sonnet, GLM, Kimi, Codex, etc.) are **really** good at navigating large codebases, provided they have the tools to do so. This project gives them these tools.

Q: No auth??

A: No. Auth is hard and you shouldn't be relying on my auth anyway. MCP standard defines auth, use that.

## Tools

All tools return structured `ToolResponse` objects with metadata (truncation status, error counts, match counts) rather than plain strings. This allows clients to programmatically detect truncation and other conditions.

<details>
### `grep`
Regex search across files using parallel directory traversal (`ignore` + `grep-searcher`).

| arg                  | type            | default | notes                                                       |
| -------------------- | --------------- | ------- | ----------------------------------------------------------- |
| `directory`          | `string`        | —       | required                                                    |
| `pattern`            | `string`        | —       | required; Rust `regex` flavor — no lookaround/backrefs      |
| `output_mode`        | `string`        | `files_with_matches` | `files_with_matches` (list matching files; fast for broad scans), `content` (matching lines with context), or `count` (per-file match tally) |
| `before_context`     | `int`           | `0`     | lines of context before matches (ignored in `files_with_matches` and `count` modes) |
| `after_context`      | `int`           | `0`     | lines of context after matches (ignored in `files_with_matches` and `count` modes) |
| `max_results`        | `int`           | `100`   | exact cap (no over-shoot); for `files_with_matches`, caps the number of files; for `content`, caps the number of matching lines; for `count`, caps the number of files |
| `case_insensitive`   | `bool`          | `false` | equivalent to `(?i)` prefix in `pattern`                    |
| `include_hidden`     | `bool`          | `false` |                                                             |
| `follow_symlinks`    | `bool`          | `false` |                                                             |
| `respect_gitignore`  | `bool`          | `true`  |                                                             |
| `file_extensions`    | `string[]`      | `[]`    | e.g. `["rs", "toml"]`; empty = all                          |
| `max_bytes`          | `int`           | ~5 MiB  | hard cap on response size                                   |

**Output modes:**
- **`files_with_matches`** (default): Returns only file paths that contain matches. Each path appears once (on first match), then the file's search stops early — efficient for broad reconnaissance queries. `max_results` caps the number of files.
- **`content`**: Returns matching lines with optional context (before/after). The classic grep output mode, useful when line-level detail is needed. `max_results` caps the number of lines.
- **`count`**: Returns per-file match tallies as `path: N` lines, sorted by path. Useful for understanding distribution of matches across files.

Walker errors and search errors are tallied and returned in the response metadata rather than silently dropped.

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
| `max_lines` | `int`    | `2000`  | maximum lines to return per call                                     |
| `max_bytes` | `int`    | ~5 MiB  | hard cap on response size (UTF-8-safe cut at line boundary)          |

Use `offset` to page through large files: if the response indicates truncation, call again with `offset = previous_offset + max_lines`. The response will include metadata indicating whether the result was truncated and the reason.
</details>

### Scope semantics

With `--project ./my/repo` set. What's ok and what isn't:

-  `cat ./my/repo/src/main.rs` — inside the root, allowed
-  `grep ./my/repo --pattern foo` — directory inside the root, allowed
-  `cat /etc/passwd` — outside the root, rejected. Nice try lol
-  `cat ./my/repo/../../etc/passwd` — canonicalizes to `/etc/passwd`, rejected. Same
-  `cat ./my/repo/link-to-secret` (symlink to `/etc/passwd`) — symlink resolves outside root, rejected. Same same.

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

### Bootstrapping memories

code-mcp is a read-only, LLM-free tool server: it can *serve* a memory directory but it cannot *create* one. Generating memories needs read access to the code, write access to the memory dir, and a model — and all three only coexist **on the box where the files live**, not in a remote client connecting over the network.

So bootstrapping is a separate, co-located step: run a local coding agent against the repo to produce the `MEMORY.md` index + per-area files, then point `--memory-dir` at the output. The included script does this:

```sh
# Uses `claude -p` by default; the agent reads/writes the local filesystem directly.
scripts/generate-memories.sh ./my/repo ./memories

# Any stdin-driven agent with local fs access works:
AGENT="codex exec" scripts/generate-memories.sh /srv/monorepo /srv/memories
```

The script feeds the agent a prompt that builds a **functional-area mental model** — the major subsystems, their entry points, how they talk, and the non-obvious gotchas — rather than transcribing code the client can already `grep`. The goal is to orient a cold client so it doesn't burn calls rediscovering structure every session. Review the generated files before serving them, then start the server with `--memory-dir ./memories`.

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
