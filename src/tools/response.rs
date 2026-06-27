//! [`ToolResponse`] — the structured output type returned by every tool.

use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::json;

/// Structured metadata returned alongside the text content of every tool call.
///
/// Serialized as the `structured_content` field of an MCP `CallToolResult`, so
/// clients can programmatically detect truncation, match counts, and errors
/// without parsing the text output.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolResponse {
    /// The text content of the tool result.
    pub content: String,
    /// Whether the output was truncated due to a size cap.
    pub truncated: bool,
    /// If truncated, the reason (e.g. "`byte_cap`", "`line_cap`").
    pub truncation_reason: Option<String>,
    /// Number of matches found (grep / find).
    pub match_count: Option<usize>,
    /// Number of walker entry errors encountered.
    pub entry_error_count: Option<usize>,
    /// Number of search errors encountered (grep only).
    pub search_error_count: Option<usize>,
    /// First error message, if any errors occurred.
    pub first_error: Option<String>,
}

impl ToolResponse {
    /// Build a minimal `ToolResponse` carrying just text content, with no
    /// truncation or match metadata. Used by tools (like `memories`) whose
    /// output is a single opaque string with no associated search metrics.
    pub fn text(content: String) -> Self {
        Self {
            content,
            truncated: false,
            truncation_reason: None,
            match_count: None,
            entry_error_count: None,
            search_error_count: None,
            first_error: None,
        }
    }

    /// Build a `CallToolResult` from this response: text content goes into
    /// `content`, and the structured metadata goes into `structured_content`.
    ///
    /// `structured_content` includes the text *plus* metadata so clients that
    /// prefer structured output still get the actual content.
    pub fn into_call_tool_result(self) -> CallToolResult {
        let structured = json!({
            "truncated": self.truncated,
            "truncation_reason": self.truncation_reason,
            "match_count": self.match_count,
            "entry_error_count": self.entry_error_count,
            "search_error_count": self.search_error_count,
            "first_error": self.first_error,
        });
        CallToolResult {
            content: vec![rmcp::model::Content::text(self.content)],
            structured_content: Some(structured),
            is_error: Some(false),
            meta: None,
        }
    }
}
