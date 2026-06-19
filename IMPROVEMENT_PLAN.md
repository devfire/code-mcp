# code-mcp Improvement Plan

Based on a thorough review of `src/main.rs` and all supporting modules, here are the
recommended improvements organized by priority and category.

---

## 1. Decompose `main.rs` — Single Responsibility

**Problem:** `main.rs` is ~430 lines and contains CLI arg definitions, three tool arg
structs, the server implementation, the `load_memory` function, the `ServerHandler`
impl, and the `main` entry point. This violates the "main should just wire things
together" principle.

**Actions:**
- [x] Move `GrepArgs`, `FindArgs`, `CatArgs`, `MemoriesArgs`, and `StringOrVec` into
      `src/tools.rs` (or a new `src/args.rs`). They are data types for the tools layer,
      not server infrastructure.
- [x] Move `load_memory` into a new `src/memory.rs` module. It has zero coupling to the
      server struct beyond `self.memory_dir`.
- [x] Move `CodeMcpServer` + its `#[tool_router]` and `#[tool_handler]` impls into
      `src/server.rs`. Keep `main.rs` to arg parsing, wiring, and `tokio::spawn`.
- [x] Result: `main.rs` becomes ~80–100 lines of pure orchestration.

---

## 2. Replace `String`-typed Tool Returns with Structured Output

**Problem:** All three tools return `Result<String, rmcp::ErrorData>`. This means:
  - No structured error information for clients.
  - Truncation markers (`... [truncated: byte cap]`) are embedded in the text —
    clients can't programmatically detect truncation.
  - The `No matches found.` sentinel is indistinguishable from a file containing
    that literal text.

**Actions:**
- [x] Define a `ToolResponse` struct with fields like `content: String`,
      `truncated: bool`, `truncation_reason: Option<String>`,
      `match_count: Option<usize>`, `error_count: Option<usize>`.
- [x] Derive `Serialize` on it so it serializes as JSON — MCP tools can return
      content blocks, not just plain text.
- [x] Update `grep`/`find`/`cat` to return `ToolResponse` instead of `String`.
- [x] Remove the `... [truncated]` and `[notice: …]` string hacks from `tools.rs`.

---

## 3. Eliminate `unwrap_or` / `unwrap_or_default` Defaults Scattered Across Call Sites

**Problem:** Default values for `max_results`, `max_bytes`, `max_lines`, etc. are
defined as `const` in `tools.rs` but applied via `unwrap_or` at every call site.
This is fragile — adding a new option means hunting down every call site.

**Actions:**
- [x] Use `#[serde(default)]` on all optional fields in the arg structs so
      deserialization fills in the defaults automatically.
- [x] Change the field types from `Option<usize>` to `usize` where a sensible
      default always exists (e.g., `max_results: usize` with `#[serde(default = "default_max_results")]`).
- [x] Remove the `unwrap_or` calls in `tools.rs` — values are already resolved.

---

## 4. Use `thiserror` Consistently — Remove Manual `map_err` Chains

**Problem:** In `main.rs`, `spawn_blocking` errors are mapped with a verbose closure:
```rust
.map_err(|e| rmcp::ErrorData::internal_error(
    "internal_error",
    Some(serde_json::json!({"error": e.to_string()})),
))?
```
This 5-line pattern is repeated identically for all four tools.

**Actions:**
- [x] Add a `From<JoinError> for rmcp::ErrorData` impl (or a helper function
      `fn join_error(e: JoinError) -> rmcp::ErrorData`) to eliminate the repetition.
- [x] Similarly, the `AppError -> ErrorData` conversion in `error.rs` already exists;
      ensure the tool methods use `?` directly instead of manual mapping where possible.
- [x] Consider a `Result<T, rmcp::ErrorData>` type alias for tool return types.

---

## 5. Builder Pattern for `CodeMcpServer`

**Problem:** `CodeMcpServer::new` takes three positional arguments. As configuration
grows (e.g., adding tool-level permissions, custom ignore patterns), the constructor
will become unwieldy.

**Actions:**
- [ ] Replace `CodeMcpServer::new(a, b, c)` with a builder:
      `CodeMcpServer::builder().memory_dir(..).scope(..).build()`.
- [ ] This also makes the `StreamableHttpService::new` closure cleaner — it calls
      `.build()` instead of cloning three args.

---

## 6. Extract Walker Configuration into a Shared Helper

**Problem:** `grep` and `find` in `tools.rs` both build a `WalkBuilder` with nearly
identical `hidden`, `git_ignore`, `git_global`, `git_exclude` settings. The only
difference is `follow_links` (grep has it, find doesn't).

**Actions:**
- [ ] Create `fn build_walker(directory: &str, opts: &WalkOpts) -> WalkBuilder`
      that centralizes the `ignore` crate configuration.
- [ ] Define a small `WalkOpts` struct shared by `GrepOptions` and `FindOptions`
      (or have them embed it via composition).
- [ ] Eliminates ~10 duplicated lines and ensures future walker config changes
      apply everywhere.

---

## 7. Replace `std::sync::mpsc` with Crossbeam in `tools.rs`

**Problem:** `grep` and `find` use `std::sync::mpsc::channel` for collecting
results from parallel walker threads. The std channel is unbounded and has
worse performance than crossbeam channels. You already depend on crossbeam-utils
(transitively) — adding `crossbeam-channel` is lightweight.

**Actions:**
- [ ] Add `crossbeam-channel` to `Cargo.toml`.
- [ ] Replace `std::sync::mpsc::{channel, Sender}` with
      `crossbeam_channel::{unbounded, Sender}`.
- [ ] Benefit: better performance, bounded option available, no API change needed.

---

## 8. Make `PeerLimiter` Eviction Configurable and Tested

**Problem:** The `evict_threshold` of 4096 is hardcoded in `per_minute()`. The
`stale_after()` eviction logic runs inline during `try_consume`, which means a
burst of requests can trigger O(n) eviction on the hot path.

**Actions:**
- [ ] Move eviction to a background task (or at least make it probabilistic /
      time-based rather than on every call past the threshold).
- [ ] Expose `evict_threshold` as a CLI arg or at least a `const` at the top of
      `limiter.rs` so it's visible and documented.
- [ ] Add a test that verifies eviction actually reclaims entries.

---

## 9. Improve Error Messages for Scope Violations

**Problem:** When `Scope::check` rejects a path, the error says
`"X is outside project root Y"`. For symlinks, this can be confusing because
the user's input path looks like it's inside the project.

**Actions:**
- [ ] Include both the original input path and the canonicalized path in the
      error message: `"symlink /project/link -> /etc/passwd resolves outside
      project root /project"`.
- [ ] This makes symlink-rejection debugging much easier for users.

---

## 10. Add `#[serde(rename_all = "snake_case")]` to Arg Structs

**Problem:** MCP clients send JSON. Without `rename_all`, serde uses the Rust
field name verbatim. Currently the fields happen to be `snake_case` already, but
adding the attribute makes the contract explicit and future-proof.

**Actions:**
- [ ] Add `#[serde(rename_all = "snake_case")]` to `GrepArgs`, `FindArgs`,
      `CatArgs`, `MemoriesArgs`.

---

## 11. Replace `StringOrVec` with a Proper Serde Helper

**Problem:** `StringOrVec` is an untagged enum that accepts either a string or
an array. This works but is a custom pattern that could be replaced with a
well-tested serde helper like `serde_with::OneOrMany` or a simple
`deserialize_with` function.

**Actions:**
- [ ] Either add `serde_with` as a dependency and use `#[serde_as(as = "OneOrMany<_>")]`,
      or write a small `deserialize_with` function.
- [ ] Remove the `StringOrVec` enum entirely — cleaner, fewer custom types.

---

## 12. Add Integration / Smoke Tests

**Problem:** The only tests are unit tests in `main.rs` (memory loading, arg
deserialization) and `tools.rs` (grep/find/cat). There are no tests for:
  - The full HTTP server round-trip (initialize → tool call → shutdown).
  - The gate middleware rejecting requests at the session cap.
  - The reaper closing idle sessions.

**Actions:**
- [ ] Add an integration test that starts the server on a random port, sends
      an MCP initialize, calls `grep`, and shuts down.
- [ ] Add a test for the gate: fill sessions to `max_sessions`, verify 503.
- [ ] Add a test for the reaper: create a session, advance time (or set a very
      short idle timeout), verify the session is closed.

---

## 13. Clippy and Rust Best Practices Pass

**Problem:** Several minor issues:
  - `format!(…)` in `MatchSink::matched` and `context` allocates on every line.
  - `String::from_utf8_lossy` creates a `Cow` every match — could be avoided
    for ASCII-heavy codebases.
  - The `let _ = match ctx.kind() { … }` in `MatchSink::context` discards the
    result but the `match` is only used for its pattern — this is confusing.

**Actions:**
- [x] Run `cargo clippy -- -W clippy::all -W clippy::pedantic` and fix findings.
- [x] Replace the `let _ = match …` with a simple `let separator = "-"` (the
      match arms all return the same value).
- [x] Use `write!` instead of `format!` + `push_str` in hot paths to avoid
      intermediate allocations.

---

## 14. Documentation and Crate-Level Docs

**Problem:** No `README.md` content beyond placeholder, no `//!` crate-level
docs, no `///` doc comments on public API in `tools.rs`.

**Actions:**
- [x] Add `//!` crate-level doc to `lib.rs` or `main.rs` explaining the
      project's purpose and architecture.
- [x] Add `///` doc comments to all public functions in `tools.rs`, `scope.rs`,
      `gate.rs`, `limiter.rs`, `reaper.rs`.
- [x] Update `README.md` with build/run instructions, CLI args, and tool
      descriptions.

---

## 15. Add `output_mode` to `grep` — `files_with_matches` / `content` / `count`

**Problem:** `grep` has exactly one output mode: dump matching lines. The consumers
are LLM agents paying per token, and their most common first query is broad
reconnaissance ("which files mention X?"). For that, full content lines are 10–50x
more tokens than needed, and when `max_results` truncates, the 100 returned lines
are arbitrary (walk order) — they may all come from the first few files the parallel
walker reached. 100 file *paths* cover essentially any realistic result set.
Claude Code's own Grep tool defaults to `files_with_matches` for exactly this reason,
so agents already expect the grep-for-files → cat workflow.

**Actions:**
- [x] Add `output_mode: String` to `GrepArgs` with values `files_with_matches`,
      `content`, `count` (serde default; reject unknown values with `invalid_params`).
- [x] `files_with_matches`: emit the path on a file's first match, then stop
      searching that file (`grep-searcher` can abort after first match — this is
      *faster* than today). `max_results` caps the number of files.
- [x] `count`: per-file match tally, output as `path: N` lines.
- [x] `content`: current behavior, unchanged.
- [x] Keep the streaming/exact-capping design intact — all modes still use the
      thread-local-buffer + mpsc pipeline; only what gets written differs.
- [x] Reuse `ToolResponse.match_count` / `truncated` for the metadata.
- [x] Default to `files_with_matches` (matches agent expectations; acceptable
      breaking change at v0.1). Document the modes in `README.md`.

---

## Priority Order

| Priority | Item | Effort | Impact |
|----------|------|--------|--------|
| 🔴 High | 15. grep output_mode | Low | Token economy / agent UX |
| 🔴 High | 1. Decompose main.rs | Medium | Maintainability |
| 🔴 High | 4. Eliminate repeated map_err | Low | DRY / readability |
| 🔴 High | 13. Clippy pass | Low | Code quality |
| 🟡 Medium | 3. Centralize defaults | Low | Correctness |
| 🟡 Medium | 6. Shared walker config | Low | DRY |
| 🟡 Medium | 9. Better scope error msgs | Low | UX |
| 🟡 Medium | 10. serde rename_all | Trivial | Robustness |
| 🟡 Medium | 14. Documentation | Medium | Adoption |
| 🟢 Low | 2. Structured tool output | High | API quality |
| 🟢 Low | 5. Builder for CodeMcpServer | Medium | Extensibility |
| 🟢 Low | 7. Crossbeam channels | Low | Performance |
| 🟢 Low | 8. PeerLimiter eviction | Medium | Robustness |
| 🟢 Low | 11. Replace StringOrVec | Low | Cleanliness |
| 🟢 Low | 12. Integration tests | High | Reliability |

---

*Generated from review of all source files on 2026-05-26.*
