# Tech Stack

- **Language**: Rust, edition 2024
- **Async runtime**: tokio 1.43 (full features)
- **MCP framework**: rmcp 0.16.0 (features: server, transport-streamable-http-server)
- **HTTP layer**: axum 0.8
- **CLI parsing**: clap 4 (derive)
- **Serialization**: serde 1.0 + serde_json 1.0 + schemars 0.8
- **Search**: grep-searcher 0.1.14 + grep-regex 0.1.13
- **File walking**: ignore 0.4.23
- **Regex**: regex 1.11
- **Error handling**: thiserror 2.0
- **Logging**: tracing 0.1 + tracing-subscriber 0.3 (env-filter)
- **Async utilities**: tokio-util 0.7
- **Dev deps**: tempfile 3, tower 0.5

Build tool: `cargo` (standard). No workspace — single crate.
