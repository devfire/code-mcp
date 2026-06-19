use crate::error::AppError;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::{WalkBuilder, WalkState};
use regex::Regex;
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024; // 5 MiB
#[cfg(test)]
const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_RESULTS: usize = 100;

// ---------------------------------------------------------------------------
// ToolResponse — structured output for all tools
// ---------------------------------------------------------------------------

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
    /// Build a `CallToolResult` from this response: text content goes into
    /// `content`, and the structured metadata goes into `structured_content`.
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

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// OutputMode for grep
// ---------------------------------------------------------------------------

/// Controls what the `grep` tool emits for each match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Emit the file path on the first match, then skip the rest of that file.
    FilesWithMatches,
    /// Emit matching lines with line numbers (the original/default behaviour).
    Content,
    /// Emit per-file match tallies as `path: N` lines.
    Count,
}

impl OutputMode {
    /// Parse a string into an `OutputMode`, returning an error for unknown values.
    pub fn from_str_lossy(s: &str) -> Result<Self, AppError> {
        match s {
            "files_with_matches" => Ok(Self::FilesWithMatches),
            "content" => Ok(Self::Content),
            "count" => Ok(Self::Count),
            other => Err(AppError::InvalidRequest(format!(
                "unknown output_mode '{other}'; expected one of: files_with_matches, content, count"
            ))),
        }
    }
}

/// Configuration for the `grep` tool. The boolean fields are independent
/// search/walker toggles; grouping them into enums would obscure the
/// (flat) JSON contract exposed to MCP clients.
#[allow(clippy::struct_excessive_bools)]
pub struct GrepOptions {
    pub before_context: usize,
    pub after_context: usize,
    pub max_results: usize,
    pub case_insensitive: bool,
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub respect_gitignore: bool,
    pub file_extensions: Vec<String>,
    pub max_bytes: usize,
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

#[derive(Clone, Copy)]
pub struct FindOptions {
    pub max_results: usize,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
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

// ---------------------------------------------------------------------------
// MatchSink (module scope)
// ---------------------------------------------------------------------------

/// Sink that accumulates matches into a per-worker `String` buffer and enforces
/// both a global match cap (via `AtomicUsize`) and a hint-level byte cap on
/// the buffer itself. The byte cap on the buffer is advisory; the authoritative
/// byte cap is enforced when draining on the main thread.
struct MatchSink<'a> {
    path: &'a Path,
    buf: &'a mut String,
    count: &'a AtomicUsize,
    max_results: usize,
    max_bytes: usize,
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

// ---------------------------------------------------------------------------
// FileMatchSink — for output_mode = files_with_matches
// ---------------------------------------------------------------------------

/// Sink that records only whether a file has at least one match. On the first
/// match it sets `matched_this_file` and returns `Ok(false)` to abort searching
/// that file (faster than continuing to read it).
struct FileMatchSink<'a> {
    count: &'a AtomicUsize,
    max_results: usize,
    matched_this_file: bool,
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

// ---------------------------------------------------------------------------
// CountSink — for output_mode = count
// ---------------------------------------------------------------------------

/// Sink that tallies matches per file. Does not emit any text during the
/// search; the per-file count is collected after the search completes.
struct CountSink {
    count: usize,
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

// ---------------------------------------------------------------------------
// Error capture helpers
// ---------------------------------------------------------------------------

fn record_first(slot: &Mutex<Option<String>>, msg: String) {
    if let Ok(mut guard) = slot.lock()
        && guard.is_none()
    {
        *guard = Some(msg);
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

#[allow(clippy::needless_pass_by_value)]
pub fn grep(
    directory: &str,
    pattern: &str,
    opts: GrepOptions,
) -> Result<ToolResponse, AppError> {
    match opts.output_mode {
        OutputMode::Content => grep_content(directory, pattern, &opts),
        OutputMode::FilesWithMatches => grep_files(directory, pattern, &opts),
        OutputMode::Count => grep_count(directory, pattern, &opts),
    }
}

/// Build a parallel walker from the shared walker options in `GrepOptions`.
fn build_parallel_walker(directory: &str, opts: &GrepOptions) -> ignore::WalkParallel {
    WalkBuilder::new(directory)
        .hidden(!opts.include_hidden)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .follow_links(opts.follow_symlinks)
        .build_parallel()
}

/// Check whether a directory entry's extension matches the filter list.
/// Returns `true` if the file should be searched.
fn extension_matches(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|w| w == e))
}

/// Collect shared error state into the final `ToolResponse` metadata fields.
fn error_metadata(
    entry_errors: &AtomicUsize,
    search_errors: &AtomicUsize,
    first_entry_err: &Mutex<Option<String>>,
    first_search_err: &Mutex<Option<String>>,
) -> (usize, usize, Option<String>) {
    let entry_err_n = entry_errors.load(Ordering::Relaxed);
    let search_err_n = search_errors.load(Ordering::Relaxed);
    let first_error = first_entry_err
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .or_else(|| first_search_err.lock().ok().and_then(|g| g.clone()));
    (entry_err_n, search_err_n, first_error)
}

/// `content` mode — the original behaviour: emit matching lines with line
/// numbers, streaming through the mpsc pipeline.
#[allow(clippy::too_many_lines)]
fn grep_content(
    directory: &str,
    pattern: &str,
    opts: &GrepOptions,
) -> Result<ToolResponse, AppError> {
    let matcher: RegexMatcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)?;

    let searcher_proto = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(opts.before_context)
        .after_context(opts.after_context)
        .build();

    let max_results = opts.max_results;
    let max_bytes = opts.max_bytes;

    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let search_errors = Arc::new(AtomicUsize::new(0));
    let first_entry_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let first_search_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let extensions = opts.file_extensions.clone();

    let walker = build_parallel_walker(directory, opts);

    let (tx, rx) = channel::<String>();

    walker.run(|| {
        let tx: Sender<String> = tx.clone();
        let count = Arc::clone(&count);
        let entry_errors = Arc::clone(&entry_errors);
        let search_errors = Arc::clone(&search_errors);
        let first_entry_err = Arc::clone(&first_entry_err);
        let first_search_err = Arc::clone(&first_search_err);
        let mut local_searcher = searcher_proto.clone();
        let local_matcher = matcher.clone();
        let extensions = extensions.clone();
        // One reusable buffer per worker thread.
        let mut buf = String::new();

        Box::new(move |result| {
            if count.load(Ordering::Relaxed) >= max_results {
                // Flush any buffered output before quitting.
                if !buf.is_empty() {
                    let _ = tx.send(mem::take(&mut buf));
                }
                return WalkState::Quit;
            }

            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    entry_errors.fetch_add(1, Ordering::Relaxed);
                    record_first(&first_entry_err, err.to_string());
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }

            if !extension_matches(entry.path(), &extensions) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let mut sink = MatchSink {
                path,
                buf: &mut buf,
                count: &count,
                max_results,
                max_bytes,
            };
            if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                search_errors.fetch_add(1, Ordering::Relaxed);
                record_first(
                    &first_search_err,
                    format!("{}: {}", path.display(), err),
                );
            }

            // Flush this worker's buffer per-file.
            if !buf.is_empty() {
                let _ = tx.send(mem::take(&mut buf));
            }

            if count.load(Ordering::Relaxed) >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    drop(tx);

    let mut output = String::new();
    let mut byte_cap_hit = false;
    while let Ok(chunk) = rx.recv() {
        if byte_cap_hit {
            continue;
        }
        if output.len() + chunk.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(output.len());
            // Cut on a UTF-8 boundary by walking back from `remaining`.
            let mut cut = remaining.min(chunk.len());
            while cut > 0 && !chunk.is_char_boundary(cut) {
                cut -= 1;
            }
            output.push_str(&chunk[..cut]);
            if !output.ends_with('\n') {
                output.push('\n');
            }
            byte_cap_hit = true;
        } else {
            output.push_str(&chunk);
        }
    }

    let (entry_err_n, search_err_n, first_error) = error_metadata(
        &entry_errors,
        &search_errors,
        &first_entry_err,
        &first_search_err,
    );
    let match_count = count.load(Ordering::Relaxed);

    Ok(ToolResponse {
        content: output,
        truncated: byte_cap_hit,
        truncation_reason: if byte_cap_hit {
            Some("byte_cap".to_string())
        } else {
            None
        },
        match_count: Some(match_count),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(search_err_n),
        first_error,
    })
}

/// `files_with_matches` mode — emit the file path on the first match, then
/// abort searching that file. `max_results` caps the number of *files*.
#[allow(clippy::too_many_lines)]
fn grep_files(
    directory: &str,
    pattern: &str,
    opts: &GrepOptions,
) -> Result<ToolResponse, AppError> {
    let matcher: RegexMatcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)?;

    // No context needed for files_with_matches; disable it for speed.
    let searcher_proto = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(false)
        .build();

    let max_results = opts.max_results;
    let max_bytes = opts.max_bytes;

    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let search_errors = Arc::new(AtomicUsize::new(0));
    let first_entry_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let first_search_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let extensions = opts.file_extensions.clone();

    let walker = build_parallel_walker(directory, opts);

    let (tx, rx) = channel::<String>();

    walker.run(|| {
        let tx: Sender<String> = tx.clone();
        let count = Arc::clone(&count);
        let entry_errors = Arc::clone(&entry_errors);
        let search_errors = Arc::clone(&search_errors);
        let first_entry_err = Arc::clone(&first_entry_err);
        let first_search_err = Arc::clone(&first_search_err);
        let mut local_searcher = searcher_proto.clone();
        let local_matcher = matcher.clone();
        let extensions = extensions.clone();

        Box::new(move |result| {
            if count.load(Ordering::Relaxed) >= max_results {
                return WalkState::Quit;
            }

            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    entry_errors.fetch_add(1, Ordering::Relaxed);
                    record_first(&first_entry_err, err.to_string());
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }

            if !extension_matches(entry.path(), &extensions) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let mut sink = FileMatchSink {
                count: &count,
                max_results,
                matched_this_file: false,
            };
            if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                search_errors.fetch_add(1, Ordering::Relaxed);
                record_first(
                    &first_search_err,
                    format!("{}: {}", path.display(), err),
                );
            }

            // If this file matched, emit its path.
            if sink.matched_this_file {
                let line = format!("{}\n", path.display());
                let _ = tx.send(line);
            }

            if count.load(Ordering::Relaxed) >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    drop(tx);

    let mut output = String::new();
    let mut byte_cap_hit = false;
    while let Ok(chunk) = rx.recv() {
        if byte_cap_hit {
            continue;
        }
        if output.len() + chunk.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(output.len());
            let mut cut = remaining.min(chunk.len());
            while cut > 0 && !chunk.is_char_boundary(cut) {
                cut -= 1;
            }
            output.push_str(&chunk[..cut]);
            if !output.ends_with('\n') {
                output.push('\n');
            }
            byte_cap_hit = true;
        } else {
            output.push_str(&chunk);
        }
    }

    let (entry_err_n, search_err_n, first_error) = error_metadata(
        &entry_errors,
        &search_errors,
        &first_entry_err,
        &first_search_err,
    );
    let match_count = count.load(Ordering::Relaxed);

    Ok(ToolResponse {
        content: output,
        truncated: byte_cap_hit,
        truncation_reason: if byte_cap_hit {
            Some("byte_cap".to_string())
        } else {
            None
        },
        match_count: Some(match_count),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(search_err_n),
        first_error,
    })
}

/// `count` mode — tally matches per file, output as `path: N` lines.
fn grep_count(
    directory: &str,
    pattern: &str,
    opts: &GrepOptions,
) -> Result<ToolResponse, AppError> {
    let matcher: RegexMatcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)?;

    // No context needed for count mode.
    let searcher_proto = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(false)
        .build();

    let max_results = opts.max_results;
    let max_bytes = opts.max_bytes;

    let entry_errors = Arc::new(AtomicUsize::new(0));
    let search_errors = Arc::new(AtomicUsize::new(0));
    let first_entry_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let first_search_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let extensions = opts.file_extensions.clone();

    // Shared map: canonical path string → match count.
    let file_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

    let walker = build_parallel_walker(directory, opts);

    walker.run(|| {
        let entry_errors = Arc::clone(&entry_errors);
        let search_errors = Arc::clone(&search_errors);
        let first_entry_err = Arc::clone(&first_entry_err);
        let first_search_err = Arc::clone(&first_search_err);
        let mut local_searcher = searcher_proto.clone();
        let local_matcher = matcher.clone();
        let extensions = extensions.clone();
        let file_counts = Arc::clone(&file_counts);

        Box::new(move |result| {
            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    entry_errors.fetch_add(1, Ordering::Relaxed);
                    record_first(&first_entry_err, err.to_string());
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }

            if !extension_matches(entry.path(), &extensions) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let mut sink = CountSink { count: 0 };
            if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                search_errors.fetch_add(1, Ordering::Relaxed);
                record_first(
                    &first_search_err,
                    format!("{}: {}", path.display(), err),
                );
            }

            if sink.count > 0 {
                let key = path.to_string_lossy().into_owned();
                let mut map = file_counts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *map.entry(key).or_insert(0) += sink.count;
            }

            WalkState::Continue
        })
    });

    // Sort by path for deterministic output.
    let mut counts_map = file_counts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut entries: Vec<_> = counts_map.drain().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let total_matches: usize = entries.iter().map(|(_, c)| *c).sum();
    let file_count = entries.len();

    // Apply max_results cap on number of files.
    let truncated = file_count > max_results;
    if truncated {
        entries.truncate(max_results);
    }

    let mut output = String::new();
    for (path, count) in &entries {
        let line = format!("{path}: {count}\n");
        if output.len() + line.len() > max_bytes {
            break;
        }
        output.push_str(&line);
    }

    let (entry_err_n, search_err_n, first_error) = error_metadata(
        &entry_errors,
        &search_errors,
        &first_entry_err,
        &first_search_err,
    );

    Ok(ToolResponse {
        content: output,
        truncated,
        truncation_reason: if truncated {
            Some("max_results".to_string())
        } else {
            None
        },
        match_count: Some(total_matches),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(search_err_n),
        first_error,
    })
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

pub fn find(
    directory: &str,
    pattern: &str,
    opts: FindOptions,
) -> Result<ToolResponse, AppError> {
    let re = Regex::new(pattern)?;
    let max_results = opts.max_results;
    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let walker = WalkBuilder::new(directory)
        .hidden(!opts.include_hidden)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .build_parallel();

    let (tx, rx) = channel::<String>();
    let match_basename = opts.match_basename;

    walker.run(|| {
        let tx = tx.clone();
        let count = Arc::clone(&count);
        let entry_errors = Arc::clone(&entry_errors);
        let first_error = Arc::clone(&first_error);
        let re = re.clone();
        let mut buf = String::new();

        Box::new(move |result| {
            if count.load(Ordering::Relaxed) >= max_results {
                if !buf.is_empty() {
                    let _ = tx.send(mem::take(&mut buf));
                }
                return WalkState::Quit;
            }

            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    entry_errors.fetch_add(1, Ordering::Relaxed);
                    record_first(&first_error, err.to_string());
                    return WalkState::Continue;
                }
            };

            let path = entry.path();
            let hay: std::borrow::Cow<'_, str> = if match_basename {
                match path.file_name() {
                    Some(name) => name.to_string_lossy(),
                    None => return WalkState::Continue,
                }
            } else {
                path.to_string_lossy()
            };

            if re.is_match(&hay) {
                let prev = count.fetch_add(1, Ordering::Relaxed);
                if prev >= max_results {
                    return WalkState::Quit;
                }
                buf.push_str(&path.to_string_lossy());
                buf.push('\n');
                let _ = tx.send(mem::take(&mut buf));
            }

            if count.load(Ordering::Relaxed) >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    drop(tx);

    let mut output = String::new();
    while let Ok(chunk) = rx.recv() {
        output.push_str(&chunk);
    }

    let entry_err_n = entry_errors.load(Ordering::Relaxed);
    let first_error = first_error
        .lock()
        .ok()
        .and_then(|g| g.clone());

    let match_count = count.load(Ordering::Relaxed);

    Ok(ToolResponse {
        content: output,
        truncated: false,
        truncation_reason: None,
        match_count: Some(match_count),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(0),
        first_error,
    })
}

// ---------------------------------------------------------------------------
// cat
// ---------------------------------------------------------------------------

pub fn cat(
    file_path: &str,
    offset: usize,
    max_lines: usize,
    max_bytes: usize,
) -> Result<ToolResponse, AppError> {
    let path = PathBuf::from(file_path);
    if !path.is_file() {
        return Err(AppError::InvalidRequest(
            "Target is not a file or does not exist".to_string(),
        ));
    }

    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);

    // Skip `offset` lines.
    let mut skip_buf = String::new();
    for _ in 0..offset {
        skip_buf.clear();
        let n = reader.read_line(&mut skip_buf)?;
        if n == 0 {
            // EOF before reaching the offset — nothing to return.
            return Ok(ToolResponse {
                content: String::new(),
                truncated: false,
                truncation_reason: None,
                match_count: None,
                entry_error_count: None,
                search_error_count: None,
                first_error: None,
            });
        }
    }

    let mut output = String::new();
    let mut line_count = 0usize;
    let mut truncated = false;
    let mut truncation_reason: Option<String> = None;
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        if line_count >= max_lines {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            truncated = true;
            truncation_reason = Some("line_cap".to_string());
            break;
        }
        if output.len() + buf.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(output.len());
            let mut cut = remaining.min(buf.len());
            while cut > 0 && !buf.is_char_boundary(cut) {
                cut -= 1;
            }
            output.push_str(&buf[..cut]);
            if !output.ends_with('\n') {
                output.push('\n');
            }
            truncated = true;
            truncation_reason = Some("byte_cap".to_string());
            break;
        }
        output.push_str(&buf);
        line_count += 1;
    }

    Ok(ToolResponse {
        content: output,
        truncated,
        truncation_reason,
        match_count: None,
        entry_error_count: None,
        search_error_count: None,
        first_error: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn write_file(dir: &Path, name: &str, contents: &str) -> std::io::Result<()> {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(path)?;
        f.write_all(contents.as_bytes())?;
        Ok(())
    }

    fn path_str(p: &Path) -> Result<&str, Box<dyn std::error::Error>> {
        p.to_str().ok_or_else(|| "non-utf8 path".into())
    }

    #[test]
    fn grep_respects_max_results_cap() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        for i in 0..50 {
            write_file(root, &format!("f{}.txt", i), "needle here\n")?;
        }
        let opts = GrepOptions {
            max_results: 10,
            respect_gitignore: false,
            ..Default::default()
        };
        let res = grep(path_str(root)?, "needle", opts)?;
        // The parallel walker uses fetch_add which can overshoot by a small
        // margin, so we verify the cap is approximately respected rather than
        // asserting an exact count.
        assert!(
            res.match_count.unwrap() <= 15,
            "expected match_count <= 15, got {:?}",
            res.match_count
        );
        assert!(!res.truncated);
        Ok(())
    }

    #[test]
    fn grep_case_insensitive_toggle() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        write_file(root, "a.txt", "Hello World\n")?;

        let case_sensitive = grep(
            path_str(root)?,
            "hello",
            GrepOptions {
                output_mode: OutputMode::Content,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert_eq!(case_sensitive.match_count, Some(0), "got {}", case_sensitive.content);

        let case_insensitive = grep(
            path_str(root)?,
            "hello",
            GrepOptions {
                case_insensitive: true,
                output_mode: OutputMode::Content,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            case_insensitive.content.contains("Hello World"),
            "got {}",
            case_insensitive.content
        );
        Ok(())
    }

    #[test]
    fn grep_filters_by_extension() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        write_file(root, "a.rs", "fn target() {}\n")?;
        write_file(root, "b.txt", "fn target() {}\n")?;

        let res = grep(
            path_str(root)?,
            "target",
            GrepOptions {
                file_extensions: vec!["rs".to_string()],
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.content.contains("a.rs"), "got {}", res.content);
        assert!(!res.content.contains("b.txt"), "got {}", res.content);
        Ok(())
    }

    #[test]
    fn grep_respects_gitignore() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        fs::create_dir_all(root.join(".git"))?;
        write_file(root, ".gitignore", "secrets.txt\n")?;
        write_file(root, "secrets.txt", "needle\n")?;
        write_file(root, "open.txt", "needle\n")?;

        let respected = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                respect_gitignore: true,
                ..Default::default()
            },
        )?;
        assert!(!respected.content.contains("secrets.txt"), "got {}", respected.content);
        assert!(respected.content.contains("open.txt"), "got {}", respected.content);

        let ignored = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(ignored.content.contains("secrets.txt"), "got {}", ignored.content);
        Ok(())
    }

    #[test]
    fn find_match_basename_and_full_path() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        write_file(root, "sub/foo.rs", "")?;
        write_file(root, "sub/bar.rs", "")?;

        let basename = find(
            path_str(root)?,
            "^foo",
            FindOptions {
                match_basename: true,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(basename.content.contains("foo.rs"), "got {}", basename.content);
        assert!(!basename.content.contains("bar.rs"), "got {}", basename.content);

        let fullpath_anchored = find(
            path_str(root)?,
            "^foo",
            FindOptions {
                match_basename: false,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert_eq!(
            fullpath_anchored.match_count,
            Some(0),
            "got {}",
            fullpath_anchored.content
        );

        let fullpath_ok = find(
            path_str(root)?,
            r"sub.*foo\.rs$",
            FindOptions {
                match_basename: false,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(fullpath_ok.content.contains("foo.rs"), "got {}", fullpath_ok.content);
        Ok(())
    }

    #[test]
    fn cat_offset_and_line_window() -> TestResult {
        let td = TempDir::new()?;
        let path = td.path().join("a.txt");
        fs::write(&path, "L1\nL2\nL3\nL4\nL5\nL6\nL7\n")?;

        let res = cat(path_str(&path)?, 2, 3, DEFAULT_MAX_BYTES)?;
        assert!(res.content.starts_with("L3\nL4\nL5\n"), "got {:?}", res.content);
        assert!(res.truncated, "expected truncated=true");
        assert_eq!(res.truncation_reason, Some("line_cap".to_string()));

        let res = cat(path_str(&path)?, 4, 3, DEFAULT_MAX_BYTES)?;
        assert_eq!(res.content, "L5\nL6\nL7\n", "got {:?}", res.content);
        assert!(!res.truncated);
        Ok(())
    }

    #[test]
    fn cat_byte_cap_truncates_with_marker() -> TestResult {
        let td = TempDir::new()?;
        let path = td.path().join("a.txt");
        let body = "abcdefghijklmnopqrstuvwxyz\n".repeat(20);
        fs::write(&path, &body)?;

        let res = cat(path_str(&path)?, 0, DEFAULT_MAX_LINES, 50)?;
        assert!(res.truncated, "expected truncated=true, got {:?}", res);
        assert_eq!(res.truncation_reason, Some("byte_cap".to_string()));
        assert!(res.content.len() < body.len(), "expected truncation, got len {}", res.content.len());
        Ok(())
    }

    #[test]
    fn cat_errors_when_path_is_directory() -> TestResult {
        let td = TempDir::new()?;
        match cat(path_str(td.path())?, 0, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES) {
            Err(AppError::InvalidRequest(_)) => Ok(()),
            Err(other) => Err(format!("expected InvalidRequest, got {:?}", other).into()),
            Ok(s) => Err(format!("expected error, got Ok({:?})", s).into()),
        }
    }

    #[test]
    fn grep_files_with_matches_mode() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        // Two files with multiple matches each.
        write_file(root, "a.txt", "needle\nneedle\nneedle\n")?;
        write_file(root, "b.txt", "no match\n")?;
        write_file(root, "c.rs", "needle here\n")?;

        let res = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                output_mode: OutputMode::FilesWithMatches,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        // Should list file paths only, not line content.
        assert!(res.content.contains("a.txt"), "got {}", res.content);
        assert!(!res.content.contains("b.txt"), "got {}", res.content);
        assert!(res.content.contains("c.rs"), "got {}", res.content);
        // No line numbers or colons (beyond the path itself).
        assert!(!res.content.contains("1:"), "should not have line numbers: {}", res.content);
        // match_count is the number of files with matches.
        assert_eq!(res.match_count, Some(2), "got {:?}", res.match_count);
        Ok(())
    }

    #[test]
    fn grep_files_with_matches_respects_max_results() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        for i in 0..20 {
            write_file(root, &format!("f{}.txt", i), "needle\n")?;
        }

        let res = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                output_mode: OutputMode::FilesWithMatches,
                max_results: 5,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        // Should cap at ~5 files.
        assert!(
            res.match_count.unwrap() <= 7,
            "expected match_count <= 7, got {:?}",
            res.match_count
        );
        Ok(())
    }

    #[test]
    fn grep_count_mode() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        write_file(root, "a.txt", "needle\nneedle\nneedle\n")?;
        write_file(root, "b.txt", "no match\n")?;
        write_file(root, "c.rs", "needle here\n")?;

        let res = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                output_mode: OutputMode::Count,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        // Should have per-file tallies.
        assert!(res.content.contains("a.txt: 3"), "got {}", res.content);
        assert!(!res.content.contains("b.txt"), "got {}", res.content);
        assert!(res.content.contains("c.rs: 1"), "got {}", res.content);
        // Total matches across all files.
        assert_eq!(res.match_count, Some(4), "got {:?}", res.match_count);
        Ok(())
    }

    #[test]
    fn grep_output_mode_rejects_unknown() -> TestResult {
        match OutputMode::from_str_lossy("bogus") {
            Err(AppError::InvalidRequest(msg)) => {
                assert!(msg.contains("bogus"), "got: {}", msg);
                Ok(())
            }
            other => Err(format!("expected InvalidRequest, got {:?}", other).into()),
        }
    }
}
