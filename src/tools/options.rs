//! Configuration types for the `grep` and `find` tools.

use super::{DEFAULT_MAX_BYTES, DEFAULT_MAX_RESULTS};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// Controls what the `grep` tool emits for each match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// Emit the file path on the first match, then skip the rest of that file.
    #[default]
    FilesWithMatches,
    /// Emit matching lines with line numbers (the original/default behaviour).
    Content,
    /// Emit per-file match tallies as `path: N` lines.
    Count,
}

/// Configuration for the `grep` tool. The boolean fields are independent
/// search/walker toggles; grouping them into enums would obscure the
/// (flat) JSON contract exposed to MCP clients.
#[allow(clippy::struct_excessive_bools)]
pub struct GrepOptions {
    /// Lines of context to emit before each match (`content` mode only).
    pub before_context: usize,
    /// Lines of context to emit after each match (`content` mode only).
    pub after_context: usize,
    /// Exact cap on results. For `files_with_matches`/`count` this caps the
    /// number of files; for `content` it caps the number of matching lines.
    pub max_results: usize,
    /// Case-insensitive matching (equivalent to a `(?i)` prefix on the pattern).
    pub case_insensitive: bool,
    /// Include hidden files and directories in the walk.
    pub include_hidden: bool,
    /// Follow symbolic links during the walk.
    pub follow_symlinks: bool,
    /// Respect `.gitignore` / global / exclude gitignore rules.
    pub respect_gitignore: bool,
    /// Restrict search to files with these extensions (empty = all files).
    pub file_extensions: Vec<String>,
    /// Hard cap on total response size in bytes.
    pub max_bytes: usize,
    /// What to emit for each match (see [`OutputMode`]).
    pub output_mode: OutputMode,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            before_context: 0,
            after_context: 0,
            max_results: DEFAULT_MAX_RESULTS,
            case_insensitive: false,
            include_hidden: false,
            follow_symlinks: false,
            respect_gitignore: true,
            file_extensions: Vec::new(),
            max_bytes: DEFAULT_MAX_BYTES,
            output_mode: OutputMode::FilesWithMatches,
        }
    }
}

impl super::common::WalkerConfig for GrepOptions {
    fn include_hidden(&self) -> bool {
        self.include_hidden
    }
    fn respect_gitignore(&self) -> bool {
        self.respect_gitignore
    }
    fn follow_symlinks(&self) -> bool {
        self.follow_symlinks
    }
}

/// Configuration for the `find` tool.
#[derive(Clone, Copy)]
pub struct FindOptions {
    /// Exact cap on the number of matching paths returned.
    pub max_results: usize,
    /// Include hidden files and directories in the walk.
    pub include_hidden: bool,
    /// Respect `.gitignore` / global / exclude gitignore rules.
    pub respect_gitignore: bool,
    /// When `true` (default), match the regex against the file's basename;
    /// when `false`, match against the full path.
    pub match_basename: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            include_hidden: false,
            respect_gitignore: true,
            match_basename: true,
        }
    }
}

impl super::common::WalkerConfig for FindOptions {
    fn include_hidden(&self) -> bool {
        self.include_hidden
    }
    fn respect_gitignore(&self) -> bool {
        self.respect_gitignore
    }
    fn follow_symlinks(&self) -> bool {
        false // find does not expose follow_symlinks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn grep_output_mode_rejects_unknown() -> TestResult {
        let result: Result<OutputMode, _> = serde_json::from_str(r#""bogus""#);
        assert!(result.is_err(), "expected deserialization error for unknown output_mode");
        Ok(())
    }

    #[test]
    fn grep_output_mode_default_is_files_with_matches() {
        assert_eq!(OutputMode::default(), OutputMode::FilesWithMatches);
    }
}
