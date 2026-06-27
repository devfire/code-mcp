//! The actual search/read implementations behind the MCP tools.
//!
//! Each tool lives in its own submodule; this module re-exports the public
//! surface (`grep`, `find`, `cat`, and their option/response types) consumed by
//! [`crate::server`].
//!
//! - [`response`] — [`ToolResponse`], the structured output returned by every tool.
//! - [`options`] — [`GrepOptions`] / [`FindOptions`] / [`OutputMode`] config types.
//! - [`sinks`] — `grep-searcher` `Sink` impls, one per output mode.
//! - [`common`] — shared walker construction, extension filtering, error
//!   capture, and byte-capped channel draining.
//! - [`grep`] / [`find`] / [`cat`] — the tool entry points.

mod cat;
mod common;
mod find;
mod grep;
mod options;
mod response;
mod sinks;

pub use cat::cat;
pub use find::find;
pub use grep::grep;
pub use options::{FindOptions, GrepOptions, OutputMode};
pub use response::ToolResponse;

pub(crate) const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024; // 5 MiB
pub(crate) const DEFAULT_MAX_RESULTS: usize = 100;
pub(crate) const DEFAULT_MAX_LINES: usize = 2000;

/// Shared test helpers used across the per-tool test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

    pub(crate) fn write_file(dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(path)?;
        f.write_all(contents.as_bytes())?;
        Ok(())
    }
}
