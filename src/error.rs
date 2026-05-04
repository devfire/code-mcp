use rmcp::ErrorData;
use serde_json::json;
use thiserror::Error;

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
            AppError::Io(_) | AppError::Ignore(_) | AppError::Internal(_) => {
                ErrorData::internal_error(
                    "internal_error",
                    Some(json!({"error": err.to_string()})),
                )
            }
        }
    }
}
