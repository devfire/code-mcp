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

use std::num::NonZeroUsize;
pub(crate) const DEFAULT_MAX_BYTES: NonZeroUsize = NonZeroUsize::new(5 * 1024 * 1024).unwrap(); // 5 MiB
pub(crate) const DEFAULT_MAX_RESULTS: NonZeroUsize = NonZeroUsize::new(100).unwrap();
pub(crate) const DEFAULT_MAX_LINES: NonZeroUsize = NonZeroUsize::new(2000).unwrap();

/// Shared test helpers used across the per-tool test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use std::fs;
    use std::io::Write;
    use std::num::NonZeroUsize;
    use std::path::Path;

    pub(crate) type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Build a [`NonZeroUsize`] from a test literal.
    pub(crate) fn nz(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).expect("test literal must be nonzero")
    }

    /// Obtain a [`ScopedPath`] for a tool entry point by running `target`
    /// through a real [`Scope`] rooted at `root` — the same validation the
    /// server performs per request.
    pub(crate) fn scoped_in(root: &Path, target: &Path) -> crate::scope::ScopedPath {
        crate::scope::Scope::new(root)
            .and_then(|s| s.check(target))
            .expect("tempdir path should canonicalize inside its own scope")
    }

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
