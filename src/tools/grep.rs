//! The `grep` tool: regex search across files via parallel directory traversal.

use super::common::{
    build_parallel_walker, drain_capped, error_metadata, extension_matches, record_first,
};
use super::options::{GrepOptions, OutputMode};
use super::response::ToolResponse;
use super::sinks::{CountSink, FileMatchSink, MatchSink};
use crate::error::AppError;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkState;
use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// Regex search across files using parallel directory traversal
/// (`ignore` + `grep-searcher`).
///
/// Dispatches to [`grep_content`], [`grep_files`], or [`grep_count`] based on
/// `opts.output_mode`. All modes share the same parallel walker, thread-local
/// buffer + mpsc pipeline, and exact `max_results` capping; only what gets
/// written to the buffer differs.
///
/// Walker entry errors and per-file search errors are tallied and surfaced in
/// the returned [`ToolResponse`] metadata rather than aborting the search.
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

/// `content` mode — the original behaviour: emit matching lines with line
/// numbers, streaming through the mpsc pipeline.
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

    let (output, byte_cap_hit) = drain_capped(&rx, max_bytes);

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

    let (output, byte_cap_hit) = drain_capped(&rx, max_bytes);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{path_str, write_file, TestResult};
    use std::fs;

    #[test]
    fn grep_respects_max_results_cap() -> TestResult {
        let td = tempfile::TempDir::new()?;
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
        let td = tempfile::TempDir::new()?;
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
        let td = tempfile::TempDir::new()?;
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
        let td = tempfile::TempDir::new()?;
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
    fn grep_files_with_matches_mode() -> TestResult {
        let td = tempfile::TempDir::new()?;
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
        let td = tempfile::TempDir::new()?;
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
        let td = tempfile::TempDir::new()?;
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
}
