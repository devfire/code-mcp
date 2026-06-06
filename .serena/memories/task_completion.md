# Task Completion Checklist

Run these before marking any coding task done:

```
cargo fmt
cargo clippy -- -D warnings
cargo test
```

All three must pass cleanly. No warnings allowed (clippy `-D warnings`).
