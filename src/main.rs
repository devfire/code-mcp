mod error;
mod tools;

use rmcp::{
    handler::server::wrapper::Parameters, schemars, tool, tool_router, tool_handler, ServerHandler,
    ServiceExt, transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GrepArgs {
    #[schemars(description = "Directory to search in")]
    directory: String,
    #[schemars(description = "Regex pattern to search for")]
    pattern: String,
    #[schemars(description = "Number of lines of before context")]
    before_context: Option<usize>,
    #[schemars(description = "Number of lines of after context")]
    after_context: Option<usize>,
    #[schemars(description = "Maximum number of results to return")]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct FindArgs {
    #[schemars(description = "Directory to search in")]
    directory: String,
    #[schemars(description = "Regex pattern to match filenames against")]
    pattern: String,
    #[schemars(description = "Maximum number of results to return")]
    max_results: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CatArgs {
    #[schemars(description = "Path to the file to read")]
    file_path: String,
    #[schemars(description = "Maximum number of lines to return")]
    max_lines: Option<usize>,
}

#[derive(Clone)]
struct CodeMcpServer {
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl CodeMcpServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl CodeMcpServer {
    #[tool(description = "Search for a regex pattern in files using blazing fast parallel directory traversal")]
    async fn grep(&self, Parameters(args): Parameters<GrepArgs>) -> Result<String, rmcp::ErrorData> {
        let res = tokio::task::spawn_blocking(move || {
            tools::grep(
                &args.directory,
                &args.pattern,
                args.before_context.unwrap_or(0),
                args.after_context.unwrap_or(0),
                args.max_results,
            )
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error("internal_error", Some(serde_json::json!({"error": e.to_string()}))))??;
        
        Ok(res)
    }

    #[tool(description = "Find files by regex pattern in a directory")]
    async fn find(&self, Parameters(args): Parameters<FindArgs>) -> Result<String, rmcp::ErrorData> {
        let res = tokio::task::spawn_blocking(move || {
            tools::find(&args.directory, &args.pattern, args.max_results)
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error("internal_error", Some(serde_json::json!({"error": e.to_string()}))))??;

        Ok(res)
    }

    #[tool(description = "Read file contents with optional line limits")]
    async fn cat(&self, Parameters(args): Parameters<CatArgs>) -> Result<String, rmcp::ErrorData> {
        let res = tokio::task::spawn_blocking(move || {
            tools::cat(&args.file_path, args.max_lines)
        })
        .await
        .map_err(|e| rmcp::ErrorData::internal_error("internal_error", Some(serde_json::json!({"error": e.to_string()}))))??;

        Ok(res)
    }
}

#[tool_handler]
impl ServerHandler for CodeMcpServer {
    fn get_info(&self) -> rmcp::model::InitializeResult {
        rmcp::model::InitializeResult {
            protocol_version: rmcp::model::ProtocolVersion::V_2024_11_05,
            server_info: rmcp::model::Implementation {
                name: "code-intelligence".into(),
                version: "0.1.0".into(),
                ..Default::default()
            },
            capabilities: rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = CodeMcpServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
