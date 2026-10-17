//! The `grep` tool: regex search across files via parallel directory traversal.

use super::common::{build_parallel_walker, drain_capped, extension_matches, record_first};
use super::options::{GrepOptions, OutputMode};
use super::response::{ToolResponse, TruncationReason};
use super::sinks::{CountSink, FileMatchSink, MatchSink};
use crate::error::AppError;
use crate::scope::ScopedPath;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkState;
use std::collections::HashMap;
use std::mem;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

// ─── Shared error tracking ──────────────────────────────────────────────────

/// Bundles the atomic counters and first-error slots shared across walker
/// threads. Cloning is cheap (all fields are `Arc`).
#[derive(Clone)]
struct ErrorState {
    entry_errors: Arc<AtomicUsize>,
    search_errors: Arc<AtomicUsize>,
    first_entry_err: Arc<Mutex<Option<String>>>,
    first_search_err: Arc<Mutex<Option<String>>>,
}

impl ErrorState {
    fn new() -> Self {
        Self {
            entry_errors: Arc::new(AtomicUsize::new(0)),
            search_errors: Arc::new(AtomicUsize::new(0)),
            first_entry_err: Arc::new(Mutex::new(None)),
            first_search_err: Arc::new(Mutex::new(None)),
        }
    }

    fn record_entry_error(&self, err: &dyn std::fmt::Display) {
        self.entry_errors.fetch_add(1, Ordering::Relaxed);
        record_first(&self.first_entry_err, err.to_string());
    }

    fn record_search_error(&self, path: &Path, err: &dyn std::fmt::Display) {
        self.search_errors.fetch_add(1, Ordering::Relaxed);
        record_first(
            &self.first_search_err,
            format!("{}: {}", path.display(), err),
        );
    }

    fn into_metadata(self) -> (usize, usize, Option<String>) {
        let entry_err_n = self.entry_errors.load(Ordering::Relaxed);
        let search_err_n = self.search_errors.load(Ordering::Relaxed);
        let first_error = self
            .first_entry_err
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .or_else(|| self.first_search_err.lock().ok().and_then(|g| g.clone()));
        (entry_err_n, search_err_n, first_error)
    }
}

// ─── Public entry point ─────────────────────────────────────────────────────

/// Regex search across files using parallel directory traversal
/// (`ignore` + `grep-searcher`).
///
/// Dispatches to the appropriate output mode. All modes share the same parallel
/// walker, error-tracking, and extension filtering; only what gets written to
/// output differs.
///
/// Walker entry errors and per-file search errors are tallied and surfaced in
/// the returned [`ToolResponse`] metadata rather than aborting the search.
#[allow(clippy::needless_pass_by_value)]
pub fn grep(
    directory: &ScopedPath,
    pattern: &str,
    opts: GrepOptions,
) -> Result<ToolResponse, AppError> {
    match opts.output_mode {
        OutputMode::Content => grep_streamed(directory, pattern, &opts, StreamMode::Content),
        OutputMode::FilesWithMatches => {
            grep_streamed(directory, pattern, &opts, StreamMode::FilesWithMatches)
        }
        OutputMode::Count => grep_count(directory, pattern, &opts),
    }
}

// ─── Streamed modes (content + files_with_matches) ──────────────────────────

/// Distinguishes the two modes that use a channel to stream results.
#[derive(Clone, Copy)]
enum StreamMode {
    Content,
    FilesWithMatches,
}

/// Unified implementation for `content` and `files_with_matches` modes.
/// Both use the mpsc pipeline with early-quit on `max_results`.
fn grep_streamed(
    directory: &ScopedPath,
    pattern: &str,
    opts: &GrepOptions,
    mode: StreamMode,
) -> Result<ToolResponse, AppError> {
    let matcher = build_matcher(pattern, opts)?;
    let searcher_proto = build_searcher(opts, &mode);
    let max_results = opts.max_results.get();
    let max_bytes = opts.max_bytes.get();
    let errors = ErrorState::new();
    let count = Arc::new(AtomicUsize::new(0));
    let extensions = opts.file_extensions.clone();
    let walker = build_parallel_walker(directory.as_ref(), opts);
    let (tx, rx) = channel::<String>();
    // Owned per-walk prefix for the per-entry scope probe below. `directory`
    // is already canonical (via `Scope::check`), so `contains` enforces that
    // each walked file still resolves inside the requested dir — closing the
    // `follow_symlinks` escape where only the walk root was checked.
    let scope_dir = directory.clone();

    walker.run(|| {
        let tx: Sender<String> = tx.clone();
        let count = Arc::clone(&count);
        let errors = errors.clone();
        let scope_dir = scope_dir.clone();
        let mut local_searcher = searcher_proto.clone();
        let local_matcher = matcher.clone();
        let extensions = extensions.clone();
        let mut buf = String::new();

        Box::new(move |result| {
            if count.load(Ordering::Relaxed) >= max_results {
                flush(&tx, &mut buf);
                return WalkState::Quit;
            }

            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    errors.record_entry_error(&err);
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            if !extension_matches(entry.path(), &extensions) {
                return WalkState::Continue;
            }
            // Per-entry scope enforcement: skip symlinks (or raced renames)
            // that resolve outside the requested directory, even when the
            // walker follows links. Canonicalize failure => skip.
            if !scope_dir.contains(entry.path()) {
                return WalkState::Continue;
            }

            let path = entry.path();

            match mode {
                StreamMode::Content => {
                    let mut sink = MatchSink {
                        path,
                        buf: &mut buf,
                        count: &count,
                        max_results,
                        max_bytes,
                    };
                    if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                        errors.record_search_error(path, &err);
                    }
                    flush(&tx, &mut buf);
                }
                StreamMode::FilesWithMatches => {
                    let mut sink = FileMatchSink {
                        count: &count,
                        max_results,
                        matched_this_file: false,
                        claimed_slot: false,
                    };
                    if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                        errors.record_search_error(path, &err);
                    }
                    if sink.claimed_slot {
                        let _ = tx.send(format!("{}\n", path.display()));
                    }
                }
            }

            if count.load(Ordering::Relaxed) >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    drop(tx);

    let (output, byte_cap_hit) = drain_capped(&rx, opts.max_bytes);
    let (entry_err_n, search_err_n, first_error) = errors.into_metadata();
    let match_count = count.load(Ordering::Relaxed);
    // `claim_slot` keeps count <= max_results, so reaching the cap means the
    // walk quit early — unless the true total is exactly the cap, which is
    // indistinguishable without completing the scan. Report MaxResults in
    // both cases (documented on `TruncationReason::MaxResults`).
    let max_results_hit = match_count >= max_results;

    Ok(ToolResponse {
        content: output,
        truncated: byte_cap_hit || max_results_hit,
        truncation_reason: if byte_cap_hit {
            Some(TruncationReason::ByteCap)
        } else if max_results_hit {
            Some(TruncationReason::MaxResults)
        } else {
            None
        },
        match_count: Some(match_count),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(search_err_n),
        first_error,
    })
}

/// Flush the per-worker buffer through the channel if non-empty.
#[inline]
fn flush(tx: &Sender<String>, buf: &mut String) {
    if !buf.is_empty() {
        let _ = tx.send(mem::take(buf));
    }
}

// ─── Count mode ─────────────────────────────────────────────────────────────

/// `count` mode — tally matches per file, output as `path: N` lines.
/// Does not use a channel; collects into a shared HashMap instead.
fn grep_count(
    directory: &ScopedPath,
    pattern: &str,
    opts: &GrepOptions,
) -> Result<ToolResponse, AppError> {
    let matcher = build_matcher(pattern, opts)?;
    let searcher_proto = build_searcher(opts, &StreamMode::FilesWithMatches); // no context, no line numbers
    let max_results = opts.max_results.get();
    let max_bytes = opts.max_bytes.get();
    let errors = ErrorState::new();
    let extensions = opts.file_extensions.clone();
    let file_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let file_count = Arc::new(AtomicUsize::new(0));
    let walker = build_parallel_walker(directory.as_ref(), opts);
    let scope_dir = directory.clone();

    walker.run(|| {
        let errors = errors.clone();
        let mut local_searcher = searcher_proto.clone();
        let local_matcher = matcher.clone();
        let extensions = extensions.clone();
        let file_counts = Arc::clone(&file_counts);
        let file_count = Arc::clone(&file_count);
        let scope_dir = scope_dir.clone();

        Box::new(move |result| {
            if file_count.load(Ordering::Relaxed) >= max_results {
                return WalkState::Quit;
            }

            let entry = match result {
                Ok(e) => e,
                Err(err) => {
                    errors.record_entry_error(&err);
                    return WalkState::Continue;
                }
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            if !extension_matches(entry.path(), &extensions) {
                return WalkState::Continue;
            }
            if !scope_dir.contains(entry.path()) {
                return WalkState::Continue;
            }

            let path = entry.path();
            let mut sink = CountSink { count: 0 };
            if let Err(err) = local_searcher.search_path(&local_matcher, path, &mut sink) {
                errors.record_search_error(path, &err);
            }

            if sink.count > 0 {
                file_count.fetch_add(1, Ordering::Relaxed);
                let key = path.to_string_lossy().into_owned();
                let mut map = file_counts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *map.entry(key).or_insert(0) += sink.count;
            }

            if file_count.load(Ordering::Relaxed) >= max_results {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });

    // Sort by path for deterministic output.
    let mut counts_map = file_counts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut entries: Vec<_> = counts_map.drain().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Reaching the cap means the walk quit early — unless the true total is
    // exactly the cap (indistinguishable without a full scan; see streamed).
    let max_results_hit = entries.len() >= max_results;
    if max_results_hit {
        entries.truncate(max_results);
    }

    let mut output = String::new();
    let mut byte_cap_hit = false;
    // Count only emitted entries, so `match_count` never includes files
    // dropped by either cap.
    let mut total_matches = 0usize;
    for (path, count) in &entries {
        let line = format!("{path}: {count}\n");
        if output.len() + line.len() > max_bytes {
            byte_cap_hit = true;
            break;
        }
        output.push_str(&line);
        total_matches += count;
    }
    let truncated = byte_cap_hit || max_results_hit;

    let (entry_err_n, search_err_n, first_error) = errors.into_metadata();

    Ok(ToolResponse {
        content: output,
        truncated,
        truncation_reason: if byte_cap_hit {
            Some(TruncationReason::ByteCap)
        } else if truncated {
            Some(TruncationReason::MaxResults)
        } else {
            None
        },
        match_count: Some(total_matches),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(search_err_n),
        first_error,
    })
}

// ─── Shared builder helpers ─────────────────────────────────────────────────

fn build_matcher(pattern: &str, opts: &GrepOptions) -> Result<RegexMatcher, AppError> {
    Ok(RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)?)
}

fn build_searcher(opts: &GrepOptions, mode: &StreamMode) -> grep_searcher::Searcher {
    let mut builder = SearcherBuilder::new();
    builder.binary_detection(BinaryDetection::quit(b'\x00'));

    match mode {
        StreamMode::Content => {
            builder
                .line_number(true)
                .before_context(opts.before_context)
                .after_context(opts.after_context);
        }
        StreamMode::FilesWithMatches => {
            builder.line_number(false);
        }
    }

    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{TestResult, nz, scoped_in, write_file};
    use std::fs;

    #[test]
    fn grep_respects_max_results_cap() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        for i in 0..50 {
            write_file(root, &format!("f{}.txt", i), "needle here\n")?;
        }
        let opts = GrepOptions {
            output_mode: OutputMode::Content,
            max_results: nz(10),
            respect_gitignore: false,
            ..Default::default()
        };
        let res = grep(&scoped_in(root, root), "needle", opts)?;
        assert_eq!(res.match_count, Some(10));
        assert_eq!(res.content.lines().count(), 10, "got {}", res.content);
        assert!(res.truncated);
        assert_eq!(res.truncation_reason, Some(TruncationReason::MaxResults));
        Ok(())
    }

    #[test]
    fn grep_case_insensitive_toggle() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        write_file(root, "a.txt", "Hello World\n")?;

        let case_sensitive = grep(
            &scoped_in(root, root),
            "hello",
            GrepOptions {
                output_mode: OutputMode::Content,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert_eq!(
            case_sensitive.match_count,
            Some(0),
            "got {}",
            case_sensitive.content
        );

        let case_insensitive = grep(
            &scoped_in(root, root),
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
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        write_file(root, "a.rs", "fn target() {}\n")?;
        write_file(root, "b.txt", "fn target() {}\n")?;

        let res = grep(
            &scoped_in(root, root),
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
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        fs::create_dir_all(root.join(".git"))?;
        write_file(root, ".gitignore", "secrets.txt\n")?;
        write_file(root, "secrets.txt", "needle\n")?;
        write_file(root, "open.txt", "needle\n")?;

        let respected = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                respect_gitignore: true,
                ..Default::default()
            },
        )?;
        assert!(
            !respected.content.contains("secrets.txt"),
            "got {}",
            respected.content
        );
        assert!(
            respected.content.contains("open.txt"),
            "got {}",
            respected.content
        );

        let ignored = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            ignored.content.contains("secrets.txt"),
            "got {}",
            ignored.content
        );
        Ok(())
    }

    #[test]
    fn grep_files_with_matches_mode() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        write_file(root, "a.txt", "needle\nneedle\nneedle\n")?;
        write_file(root, "b.txt", "no match\n")?;
        write_file(root, "c.rs", "needle here\n")?;

        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                output_mode: OutputMode::FilesWithMatches,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.content.contains("a.txt"), "got {}", res.content);
        assert!(!res.content.contains("b.txt"), "got {}", res.content);
        assert!(res.content.contains("c.rs"), "got {}", res.content);
        assert!(
            !res.content.contains("1:"),
            "should not have line numbers: {}",
            res.content
        );
        assert_eq!(res.match_count, Some(2), "got {:?}", res.match_count);
        Ok(())
    }

    #[test]
    fn grep_files_with_matches_respects_max_results() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        for i in 0..20 {
            write_file(root, &format!("f{}.txt", i), "needle\n")?;
        }

        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                output_mode: OutputMode::FilesWithMatches,
                max_results: nz(5),
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert_eq!(res.match_count, Some(5));
        assert_eq!(res.content.lines().count(), 5, "got {}", res.content);
        assert!(res.truncated);
        assert_eq!(res.truncation_reason, Some(TruncationReason::MaxResults));
        Ok(())
    }

    #[test]
    fn grep_count_mode_cap_counts_only_emitted_files() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        for i in 0..20 {
            write_file(root, &format!("f{}.txt", i), "needle\nneedle\n")?;
        }
        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                output_mode: OutputMode::Count,
                max_results: nz(5),
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert_eq!(res.content.lines().count(), 5, "got {}", res.content);
        // 5 emitted files x 2 matches each; dropped files must not count.
        assert_eq!(res.match_count, Some(10));
        assert!(res.truncated);
        assert_eq!(res.truncation_reason, Some(TruncationReason::MaxResults));
        Ok(())
    }

    #[test]
    fn grep_count_mode_byte_cap_sets_truncated_and_counts_only_emitted() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        for i in 0..20 {
            write_file(root, &format!("f{:02}.txt", i), "needle\nneedle\n")?;
        }
        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                output_mode: OutputMode::Count,
                max_bytes: nz(30),
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.truncated, "got {:?}", res.truncation_reason);
        assert_eq!(res.truncation_reason, Some(TruncationReason::ByteCap));
        assert!(res.content.len() <= 30, "got {}", res.content.len());
        // match_count sums only the emitted `path: N` lines (2 per file).
        let emitted: usize = res
            .content
            .lines()
            .filter_map(|l| l.rsplit(": ").next()?.parse::<usize>().ok())
            .sum();
        assert_eq!(res.match_count, Some(emitted));
        assert!(emitted < 40, "byte cap must drop files, emitted={emitted}");
        Ok(())
    }

    #[test]
    fn grep_count_mode() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        write_file(root, "a.txt", "needle\nneedle\nneedle\n")?;
        write_file(root, "b.txt", "no match\n")?;
        write_file(root, "c.rs", "needle here\n")?;

        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                output_mode: OutputMode::Count,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.content.contains("a.txt: 3"), "got {}", res.content);
        assert!(!res.content.contains("b.txt"), "got {}", res.content);
        assert!(res.content.contains("c.rs: 1"), "got {}", res.content);
        assert_eq!(res.match_count, Some(4), "got {:?}", res.match_count);
        Ok(())
    }

    #[test]
    fn grep_skips_symlinked_file_pointing_outside_even_when_following() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        let outside = tempfile::TempDir::new()?;
        fs::write(outside.path().join("secret.txt"), "needle outside\n")?;
        write_file(root, "inside.txt", "needle inside\n")?;
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("link.txt"))?;

        for mode in [
            OutputMode::Content,
            OutputMode::FilesWithMatches,
            OutputMode::Count,
        ] {
            let res = grep(
                &scoped_in(root, root),
                "needle",
                GrepOptions {
                    output_mode: mode,
                    follow_symlinks: true,
                    respect_gitignore: false,
                    ..Default::default()
                },
            )?;
            assert!(
                res.content.contains("inside.txt"),
                "mode {mode:?}: got {}",
                res.content
            );
            assert!(
                !res.content.contains("secret.txt"),
                "mode {mode:?}: leaked outside: {}",
                res.content
            );
            assert!(
                !res.content.contains("needle outside"),
                "mode {mode:?}: leaked content: {}",
                res.content
            );
        }
        Ok(())
    }

    #[test]
    fn grep_skips_symlinked_dir_pointing_outside_when_following() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        let outside = tempfile::TempDir::new()?;
        fs::write(outside.path().join("secret.txt"), "needle outside\n")?;
        write_file(root, "inside.txt", "needle inside\n")?;
        std::os::unix::fs::symlink(outside.path(), root.join("linkdir"))?;

        let res = grep(
            &scoped_in(root, root),
            "needle",
            GrepOptions {
                follow_symlinks: true,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.content.contains("inside.txt"), "got {}", res.content);
        assert!(
            !res.content.contains("needle outside"),
            "leaked: {}",
            res.content
        );
        Ok(())
    }
}
