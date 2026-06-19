use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Default value functions for serde(default = "…")
// ---------------------------------------------------------------------------

const fn default_zero() -> usize {
    0
}
const fn default_max_results() -> usize {
    100
}
const fn default_max_bytes() -> usize {
    5 * 1024 * 1024 // 5 MiB
}
const fn default_max_lines() -> usize {
    2000
}
const fn default_false() -> bool {
    false
}
const fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// StringOrVec — accepts a single string or an array of strings
// ---------------------------------------------------------------------------

/// Serde helper accepting either a single string or an array of strings.
/// Used by `GrepArgs::file_extensions` so MCP clients can pass either
/// `"sql"` or `["rs", "toml"]`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StringOrVec {
    /// A single extension string.
    One(String),
    /// An array of extension strings.
    Many(Vec<String>),
}

impl StringOrVec {
    /// Normalize into a `Vec<String>` regardless of which variant was
    /// deserialized.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StringOrVec::One(s) => vec![s],
            StringOrVec::Many(v) => v,
        }
    }
}

fn default_file_extensions() -> Option<StringOrVec> {
    None
}

fn default_output_mode() -> String {
    "files_with_matches".to_string()
}

// ---------------------------------------------------------------------------
// Arg structs — serde fills in defaults automatically
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::struct_excessive_bools)]
pub struct GrepArgs {
    #[schemars(description = "Directory to search in")]
    pub directory: String,
    #[schemars(description = "Regex pattern to search for (Rust regex; no lookaround/backrefs). Use (?i) for case-insensitive.")]
    pub pattern: String,
    #[serde(default = "default_zero")]
    #[schemars(description = "Number of lines of context before each match (default 0)")]
    pub before_context: usize,
    #[serde(default = "default_zero")]
    #[schemars(description = "Number of lines of context after each match (default 0)")]
    pub after_context: usize,
    #[serde(default = "default_max_results")]
    #[schemars(description = "Maximum number of results to return (default 100)")]
    pub max_results: usize,
    #[serde(default = "default_false")]
    #[schemars(description = "Case-insensitive search (default false). Equivalent to prefixing pattern with (?i).")]
    pub case_insensitive: bool,
    #[serde(default = "default_false")]
    #[schemars(description = "Include hidden files and directories (default false)")]
    pub include_hidden: bool,
    #[serde(default = "default_false")]
    #[schemars(description = "Follow symbolic links (default false)")]
    pub follow_symlinks: bool,
    #[serde(default = "default_true")]
    #[schemars(description = "Respect .gitignore files (default true)")]
    pub respect_gitignore: bool,
    #[serde(default = "default_file_extensions")]
    #[schemars(description = "Restrict to files with these extensions. Accepts either a single string (\"sql\") or an array ([\"rs\", \"toml\"]). Empty means all files.")]
    pub file_extensions: Option<StringOrVec>,
    #[serde(default = "default_max_bytes")]
    #[schemars(description = "Hard cap on total response size in bytes (default ~5 MiB). Truncates with a marker.")]
    pub max_bytes: usize,
    #[serde(default = "default_output_mode")]
    #[schemars(description = "Output mode: 'files_with_matches' (default — list file paths only), 'content' (matching lines with line numbers), 'count' (per-file match tallies as path: N).")]
    pub output_mode: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct FindArgs {
    #[schemars(description = "Directory to search in")]
    pub directory: String,
    #[schemars(description = "Regex pattern to match against filenames (Rust regex; no lookaround/backrefs)")]
    pub pattern: String,
    #[serde(default = "default_max_results")]
    #[schemars(description = "Maximum number of results to return (default 100)")]
    pub max_results: usize,
    #[serde(default = "default_false")]
    #[schemars(description = "Include hidden files and directories (default false)")]
    pub include_hidden: bool,
    #[serde(default = "default_true")]
    #[schemars(description = "Respect .gitignore files (default true)")]
    pub respect_gitignore: bool,
    #[serde(default = "default_true")]
    #[schemars(description = "Match the basename only (default true). Set false to match the full path.")]
    pub match_basename: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct CatArgs {
    #[schemars(description = "Path to the file to read")]
    pub file_path: String,
    #[serde(default = "default_zero")]
    #[schemars(description = "Line offset to start from (0-based, default 0). Use to paginate long files.")]
    pub offset: usize,
    #[serde(default = "default_max_lines")]
    #[schemars(description = "Maximum number of lines to return (default 2000)")]
    pub max_lines: usize,
    #[serde(default = "default_max_bytes")]
    #[schemars(description = "Maximum number of bytes to return (default ~5 MiB)")]
    pub max_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub struct MemoriesArgs {
    #[serde(default)]
    #[schemars(description = "Optional memory file name (relative to memory dir, e.g. \"user_role.md\"). If omitted, returns the index from MEMORY.md or a directory listing.")]
    pub name: Option<String>,
}
