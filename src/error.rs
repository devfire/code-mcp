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

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<AppError> for ErrorData {
    fn from(err: AppError) -> Self {
        match err {
            AppError::Io(_) | AppError::Regex(_) | AppError::GrepRegex(_) | AppError::Ignore(_) => {
                ErrorData::invalid_params("invalid_request", Some(json!({"error": err.to_string()})))
            }
            AppError::InvalidRequest(_) => {
                ErrorData::invalid_params("invalid_request", Some(json!({"error": err.to_string()})))
            }
            AppError::Internal(_) => {
                ErrorData::internal_error("internal_error", Some(json!({"error": err.to_string()})))
            }
        }
    }
}
