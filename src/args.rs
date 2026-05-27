use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StringOrVec {
    One(String),
    Many(Vec<String>),
}

impl StringOrVec {
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StringOrVec::One(s) => vec![s],
            StringOrVec::Many(v) => v,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepArgs {
    #[schemars(description = "Directory to search in")]
    pub directory: String,
    #[schemars(description = "Regex pattern to search for (Rust regex; no lookaround/backrefs). Use (?i) for case-insensitive.")]
    pub pattern: String,
    #[schemars(description = "Number of lines of context before each match (default 0)")]
    pub before_context: Option<usize>,
    #[schemars(description = "Number of lines of context after each match (default 0)")]
    pub after_context: Option<usize>,
    #[schemars(description = "Maximum number of results to return")]
    pub max_results: Option<usize>,
    #[schemars(description = "Case-insensitive search (default false). Equivalent to prefixing pattern with (?i).")]
    pub case_insensitive: Option<bool>,
    #[schemars(description = "Include hidden files and directories (default false)")]
    pub include_hidden: Option<bool>,
    #[schemars(description = "Follow symbolic links (default false)")]
    pub follow_symlinks: Option<bool>,
    #[schemars(description = "Respect .gitignore files (default true)")]
    pub respect_gitignore: Option<bool>,
    #[schemars(description = "Restrict to files with these extensions. Accepts either a single string (\"sql\") or an array ([\"rs\", \"toml\"]). Empty means all files.")]
    pub file_extensions: Option<StringOrVec>,
    #[schemars(description = "Hard cap on total response size in bytes (default ~5 MiB). Truncates with a marker.")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindArgs {
    #[schemars(description = "Directory to search in")]
    pub directory: String,
    #[schemars(description = "Regex pattern to match against filenames (Rust regex; no lookaround/backrefs)")]
    pub pattern: String,
    #[schemars(description = "Maximum number of results to return")]
    pub max_results: Option<usize>,
    #[schemars(description = "Include hidden files and directories (default false)")]
    pub include_hidden: Option<bool>,
    #[schemars(description = "Respect .gitignore files (default true)")]
    pub respect_gitignore: Option<bool>,
    #[schemars(description = "Match the basename only (default true). Set false to match the full path.")]
    pub match_basename: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CatArgs {
    #[schemars(description = "Path to the file to read")]
    pub file_path: String,
    #[schemars(description = "Line offset to start from (0-based, default 0). Use to paginate long files.")]
    pub offset: Option<usize>,
    #[schemars(description = "Maximum number of lines to return")]
    pub max_lines: Option<usize>,
    #[schemars(description = "Maximum number of bytes to return")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MemoriesArgs {
    #[schemars(description = "Optional memory file name (relative to memory dir, e.g. \"user_role.md\"). If omitted, returns the index from MEMORY.md or a directory listing.")]
    pub name: Option<String>,
}
