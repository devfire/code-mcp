use crate::error::AppError;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::{WalkBuilder, WalkState};
use regex::Regex;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

const DEFAULT_MAX_BYTES: usize = 5 * 1024 * 1024; // 5 MiB
const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_RESULTS: usize = 100;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub struct GrepOptions {
    pub before_context: usize,
    pub after_context: usize,
    pub max_results: Option<usize>,
    pub case_insensitive: bool,
    pub include_hidden: bool,
    pub follow_symlinks: bool,
    pub respect_gitignore: bool,
    pub file_extensions: Vec<String>,
    pub max_bytes: Option<usize>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            before_context: 0,
            after_context: 0,
            max_results: Some(DEFAULT_MAX_RESULTS),
            case_insensitive: false,
            include_hidden: false,
            follow_symlinks: false,
            respect_gitignore: true,
            file_extensions: Vec::new(),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        }
    }
}

pub struct FindOptions {
    pub max_results: Option<usize>,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    pub match_basename: bool,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            max_results: Some(DEFAULT_MAX_RESULTS),
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

impl<'a> Sink for MatchSink<'a> {
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
        self.buf
            .push_str(&format!("{}:{}: {}", self.path.display(), line_num, line));
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
        let _ = match ctx.kind() {
            SinkContextKind::Before | SinkContextKind::After | SinkContextKind::Other => "-",
        };
        let separator = "-";
        let line_num = ctx.line_number().unwrap_or(0);
        let line = String::from_utf8_lossy(ctx.bytes());
        self.buf.push_str(&format!(
            "{}{}{} {}",
            self.path.display(),
            separator,
            line_num,
            line
        ));
        if !line.ends_with('\n') {
            self.buf.push('\n');
        }
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

fn append_notice(
    output: &mut String,
    entry_errors: usize,
    search_errors: usize,
    first_entry: &Mutex<Option<String>>,
    first_search: &Mutex<Option<String>>,
) {
    if entry_errors == 0 && search_errors == 0 {
        return;
    }
    let first = first_entry
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .or_else(|| first_search.lock().ok().and_then(|g| g.clone()))
        .unwrap_or_else(|| "<unknown>".to_string());
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!(
        "\n[notice: {} entry errors, {} search errors; first: {}]\n",
        entry_errors, search_errors, first
    ));
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

pub fn grep(
    directory: &str,
    pattern: &str,
    opts: GrepOptions,
) -> Result<String, AppError> {
    let matcher: RegexMatcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)?;

    let searcher_proto = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(opts.before_context)
        .after_context(opts.after_context)
        .build();

    let max_results = opts.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
    let max_bytes = opts.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let search_errors = Arc::new(AtomicUsize::new(0));
    let first_entry_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let first_search_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let extensions = opts.file_extensions.clone();

    let walker = WalkBuilder::new(directory)
        .hidden(!opts.include_hidden)
        .git_ignore(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .follow_links(opts.follow_symlinks)
        .build_parallel();

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

            if !extensions.is_empty() {
                let matches_ext = entry
                    .path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| extensions.iter().any(|w| w == e))
                    .unwrap_or(false);
                if !matches_ext {
                    return WalkState::Continue;
                }
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
            output.push_str("... [truncated: byte cap]\n");
            byte_cap_hit = true;
        } else {
            output.push_str(&chunk);
        }
    }

    let entry_err_n = entry_errors.load(Ordering::Relaxed);
    let search_err_n = search_errors.load(Ordering::Relaxed);
    append_notice(
        &mut output,
        entry_err_n,
        search_err_n,
        &first_entry_err,
        &first_search_err,
    );

    if output.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// find
// ---------------------------------------------------------------------------

pub fn find(
    directory: &str,
    pattern: &str,
    opts: FindOptions,
) -> Result<String, AppError> {
    let re = Regex::new(pattern)?;
    let max_results = opts.max_results.unwrap_or(DEFAULT_MAX_RESULTS);

    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let first_entry_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let dummy_search_err: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

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
        let first_entry_err = Arc::clone(&first_entry_err);
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
                    record_first(&first_entry_err, err.to_string());
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
    append_notice(
        &mut output,
        entry_err_n,
        0,
        &first_entry_err,
        &dummy_search_err,
    );

    if output.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// cat
// ---------------------------------------------------------------------------

pub fn cat(
    file_path: &str,
    offset: usize,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<String, AppError> {
    let path = PathBuf::from(file_path);
    if !path.is_file() {
        return Err(AppError::InvalidRequest(
            "Target is not a file or does not exist".to_string(),
        ));
    }

    let max_lines = max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = max_bytes.unwrap_or(DEFAULT_MAX_BYTES);

    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);

    // Skip `offset` lines.
    let mut skip_buf = String::new();
    for _ in 0..offset {
        skip_buf.clear();
        let n = reader.read_line(&mut skip_buf)?;
        if n == 0 {
            // EOF before reaching the offset — nothing to return.
            return Ok(String::new());
        }
    }

    let mut output = String::new();
    let mut line_count = 0usize;
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
            output.push_str("... [truncated: line cap]\n");
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
            output.push_str("... [truncated: byte cap]\n");
            break;
        }
        output.push_str(&buf);
        line_count += 1;
    }

    Ok(output)
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
    fn grep_finds_matches_and_respects_max_results_exactly() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        for i in 0..50 {
            write_file(root, &format!("f{}.txt", i), "needle here\n")?;
        }
        let opts = GrepOptions {
            max_results: Some(10),
            respect_gitignore: false,
            ..Default::default()
        };
        let out = grep(path_str(root)?, "needle", opts)?;
        let match_lines: Vec<&str> = out
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with("[notice"))
            .collect();
        assert_eq!(match_lines.len(), 10, "got: {:?}", out);
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
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(case_sensitive.contains("No matches"), "got {}", case_sensitive);

        let case_insensitive = grep(
            path_str(root)?,
            "hello",
            GrepOptions {
                case_insensitive: true,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            case_insensitive.contains("Hello World"),
            "got {}",
            case_insensitive
        );
        Ok(())
    }

    #[test]
    fn grep_filters_by_extension() -> TestResult {
        let td = TempDir::new()?;
        let root = td.path();
        write_file(root, "a.rs", "fn target() {}\n")?;
        write_file(root, "b.txt", "fn target() {}\n")?;

        let out = grep(
            path_str(root)?,
            "target",
            GrepOptions {
                file_extensions: vec!["rs".to_string()],
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(out.contains("a.rs"), "got {}", out);
        assert!(!out.contains("b.txt"), "got {}", out);
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
        assert!(!respected.contains("secrets.txt"), "got {}", respected);
        assert!(respected.contains("open.txt"), "got {}", respected);

        let ignored = grep(
            path_str(root)?,
            "needle",
            GrepOptions {
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(ignored.contains("secrets.txt"), "got {}", ignored);
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
        assert!(basename.contains("foo.rs"), "got {}", basename);
        assert!(!basename.contains("bar.rs"), "got {}", basename);

        let fullpath_anchored = find(
            path_str(root)?,
            "^foo",
            FindOptions {
                match_basename: false,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            fullpath_anchored.contains("No matches"),
            "got {}",
            fullpath_anchored
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
        assert!(fullpath_ok.contains("foo.rs"), "got {}", fullpath_ok);
        Ok(())
    }

    #[test]
    fn cat_offset_and_line_window() -> TestResult {
        let td = TempDir::new()?;
        let path = td.path().join("a.txt");
        fs::write(&path, "L1\nL2\nL3\nL4\nL5\nL6\nL7\n")?;

        let out = cat(path_str(&path)?, 2, Some(3), None)?;
        assert!(out.starts_with("L3\nL4\nL5\n"), "got {:?}", out);
        assert!(out.contains("[truncated: line cap]"), "got {:?}", out);

        let out = cat(path_str(&path)?, 4, Some(3), None)?;
        assert_eq!(out, "L5\nL6\nL7\n", "got {:?}", out);
        Ok(())
    }

    #[test]
    fn cat_byte_cap_truncates_with_marker() -> TestResult {
        let td = TempDir::new()?;
        let path = td.path().join("a.txt");
        let body = "abcdefghijklmnopqrstuvwxyz\n".repeat(20);
        fs::write(&path, &body)?;

        let out = cat(path_str(&path)?, 0, None, Some(50))?;
        assert!(out.contains("[truncated: byte cap]"), "got {:?}", out);
        assert!(out.len() < body.len(), "expected truncation, got len {}", out.len());
        Ok(())
    }

    #[test]
    fn cat_errors_when_path_is_directory() -> TestResult {
        let td = TempDir::new()?;
        match cat(path_str(td.path())?, 0, None, None) {
            Err(AppError::InvalidRequest(_)) => Ok(()),
            Err(other) => Err(format!("expected InvalidRequest, got {:?}", other).into()),
            Ok(s) => Err(format!("expected error, got Ok({:?})", s).into()),
        }
    }
}
