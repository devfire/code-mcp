# Code Conventions

## Structure
- Tool argument structs live in `args.rs`; tool implementation logic lives in `tools.rs`
- Serde defaults centralized in `args.rs` as free functions (`default_zero`, `default_true`, etc.)
- `ToolResponse` is the single return type for all tool functions; never return raw strings from tools

## Naming
- Structs: PascalCase; fields: snake_case
- Modules mirror filenames (standard Rust)

## Error Handling
- Use `AppError` (thiserror) for all error variants
- `From<AppError> for ErrorData` bridges into rmcp error protocol

## Serde
- Default values declared via `#[serde(default = "fn_name")]` pointing to functions in `args.rs`
- Avoid inline `|| value` closures for serde defaults — use named functions

## No comments policy
- Minimal comments; only non-obvious invariants
