# Suggested Commands

## Build & Run
```
cargo build
cargo run -- [options]      # run MCP server
cargo run -- --help         # CLI usage
```

## Test
```
cargo test
cargo test -- --nocapture   # show println output
```

## Lint / Format
```
cargo clippy
cargo fmt
```

## Search (project-specific note)
`rg` (ripgrep) is available in this environment as a shell function wrapping the Claude Code binary; the underlying ripgrep 14.1.1 is invoked when `ARGV0=rg`. Use `rg` freely for codebase searches in terminal.
