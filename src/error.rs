use rmcp::ErrorData;
use rmcp::model::CallToolResult;
use serde_json::json;
use thiserror::Error;

/// Convert a `tokio::task::JoinError` (from `spawn_blocking`) into an
/// `rmcp::ErrorData` with `internal_error` code. Used by every tool handler
/// so the `.map_err` boilerplate is a single call.
///
/// Takes `JoinError` by value so it can be passed directly as
/// `.map_err(join_error)` — `JoinError` is not `Clone`, and the only
/// field we need is accessed via `&self`.
#[allow(clippy::needless_pass_by_value)]
pub fn join_error(e: tokio::task::JoinError) -> ErrorData {
    ErrorData::internal_error("internal_error", Some(json!({"error": e.to_string()})))
}

/// Convenience alias for tool handler return types.
pub type ToolResult<T> = Result<T, ErrorData>;

/// Convert an `AppError` into a `CallToolResult` with `is_error: true`.
/// This keeps tool failures at the tool level (the session stays alive)
/// rather than escalating to a JSON-RPC protocol error that kills the session.
pub fn tool_error(err: AppError) -> CallToolResult {
    CallToolResult {
        content: vec![rmcp::model::Content::text(err.to_string())],
        structured_content: None,
        is_error: Some(true),
        meta: None,
    }
}

/// Application-level error type. Variants map to MCP error codes via the
/// `From<AppError> for ErrorData` impl below: user-facing failures
/// (bad regex, scope violations, not-found) become `invalid_params`, while
/// infrastructure failures (I/O, ignore, axum) become `internal_error`.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("Grep regex error: {0}")]
    GrepRegex(#[from] grep_regex::Error),

    #[error("Ignore error: {0}")]
    Ignore(#[from] ignore::Error),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Out of project scope: {0}")]
    OutOfScope(String),

    #[error("Server error: {0}")]
    Axum(#[from] axum::Error),
}

impl From<AppError> for ErrorData {
    fn from(err: AppError) -> Self {
        match err {
            AppError::Regex(_)
            | AppError::GrepRegex(_)
            | AppError::InvalidRequest(_)
            | AppError::NotFound(_)
            | AppError::OutOfScope(_) => ErrorData::invalid_params(
                "invalid_params",
                Some(json!({"error": err.to_string()})),
            ),
            AppError::Io(_) | AppError::Ignore(_) | AppError::Internal(_) | AppError::Axum(_) => {
                ErrorData::internal_error(
                    "internal_error",
                    Some(json!({"error": err.to_string()})),
                )
            }
        }
    }
}
