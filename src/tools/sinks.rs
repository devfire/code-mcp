//! `grep-searcher` [`Sink`] implementations, one per [`OutputMode`].
//!
//! [`OutputMode`]: super::options::OutputMode

use grep_searcher::{Searcher, Sink, SinkContext, SinkMatch};
use std::fmt::Write;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Sink that accumulates matches into a per-worker `String` buffer and enforces
/// both a global match cap (via `AtomicUsize`) and a hint-level byte cap on
/// the buffer itself. The byte cap on the buffer is advisory; the authoritative
/// byte cap is enforced when draining on the main thread.
pub(crate) struct MatchSink<'a> {
    pub(crate) path: &'a Path,
    pub(crate) buf: &'a mut String,
    pub(crate) count: &'a AtomicUsize,
    pub(crate) max_results: usize,
    pub(crate) max_bytes: usize,
}

impl Sink for MatchSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        // Increment first; if we are over the cap, undo conceptually by stopping.
        let prev = self.count.fetch_add(1, Ordering::Relaxed);
        if prev >= self.max_results {
            return Ok(false);
        }
        if self.buf.len() >= self.max_bytes {
            return Ok(false);
        }
        let line_num = mat.line_number().unwrap_or(0);
        let line = String::from_utf8_lossy(mat.bytes());
        let _ = write!(self.buf, "{}:{}: {}", self.path.display(), line_num, line);
        if !line.ends_with('\n') {
            self.buf.push('\n');
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        // Context lines do not count toward `max_results` (the cap is on
        // matches, not surrounding lines), but we still respect the byte cap.
        if self.buf.len() >= self.max_bytes {
            return Ok(false);
        }
        // All context kinds use the same separator.
        let separator = "-";
        let line_num = ctx.line_number().unwrap_or(0);
        let line = String::from_utf8_lossy(ctx.bytes());
        let _ = write!(
            self.buf,
            "{}{}{} {}",
            self.path.display(),
            separator,
            line_num,
            line
        );
        if !line.ends_with('\n') {
            self.buf.push('\n');
        }
        Ok(true)
    }
}

/// Sink that records only whether a file has at least one match. On the first
/// match it sets `matched_this_file` and returns `Ok(false)` to abort searching
/// that file (faster than continuing to read it).
pub(crate) struct FileMatchSink<'a> {
    pub(crate) count: &'a AtomicUsize,
    pub(crate) max_results: usize,
    pub(crate) matched_this_file: bool,
}

impl Sink for FileMatchSink<'_> {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        if self.matched_this_file {
            // Already recorded this file; stop searching it.
            return Ok(false);
        }
        self.matched_this_file = true;
        let prev = self.count.fetch_add(1, Ordering::Relaxed);
        if prev >= self.max_results {
            return Ok(false);
        }
        // Stop searching this file — we only need the first match.
        Ok(false)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        _ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        // No context needed for files_with_matches mode.
        Ok(true)
    }
}

/// Sink that tallies matches per file. Does not emit any text during the
/// search; the per-file count is collected after the search completes.
pub(crate) struct CountSink {
    pub(crate) count: usize,
}

impl Sink for CountSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        _mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        self.count += 1;
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        _ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        Ok(true)
    }
}
