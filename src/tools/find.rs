//! The `find` tool: locate files by regex over a parallel `ignore` walker.

use super::common::{build_parallel_walker, record_first};
use super::options::FindOptions;
use super::response::{ToolResponse, TruncationReason};
use super::sinks::claim_slot;
use crate::error::AppError;
use crate::scope::ScopedPath;
use ignore::WalkState;
use regex::Regex;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

/// Find files by regex. Matches the basename by default; set
/// `opts.match_basename = false` to match against the full path.
///
/// Uses a parallel `ignore` walker (gitignore-aware) and an `AtomicUsize`
/// counter for exact `max_results` capping. Walker entry errors are tallied
/// and surfaced in the returned [`ToolResponse`] metadata.
pub fn find(
    directory: &ScopedPath,
    pattern: &str,
    opts: FindOptions,
) -> Result<ToolResponse, AppError> {
    let re = Regex::new(pattern)?;
    let max_results = opts.max_results.get();
    let count = Arc::new(AtomicUsize::new(0));
    let entry_errors = Arc::new(AtomicUsize::new(0));
    let first_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let walker = build_parallel_walker(directory.as_ref(), &opts);

    let (tx, rx) = channel::<String>();
    let match_basename = opts.match_basename;
    // Per-entry scope probe (see `ScopedPath::contains`): even though `find`
    // never follows symlinks at the walker level, a symlink *file* inside
    // the project could still resolve outside — skip it per entry.
    let scope_dir = directory.clone();

    walker.run(|| {
        let tx = tx.clone();
        let count = Arc::clone(&count);
        let entry_errors = Arc::clone(&entry_errors);
        let first_error = Arc::clone(&first_error);
        let scope_dir = scope_dir.clone();
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

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            if !scope_dir.contains(entry.path()) {
                return WalkState::Continue;
            }

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
                if !claim_slot(&count, max_results) {
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
    let first_error = first_error.lock().ok().and_then(|g| g.clone());

    let match_count = count.load(Ordering::Relaxed);
    // `claim_slot` keeps count <= max_results; reaching the cap means the
    // walk quit early, unless the true total is exactly the cap
    // (indistinguishable without a full scan — see `TruncationReason`).
    let max_results_hit = match_count >= max_results;

    Ok(ToolResponse {
        content: output,
        truncated: max_results_hit,
        truncation_reason: if max_results_hit {
            Some(TruncationReason::MaxResults)
        } else {
            None
        },
        match_count: Some(match_count),
        entry_error_count: Some(entry_err_n),
        search_error_count: Some(0),
        first_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{TestResult, nz, scoped_in, write_file};

    #[test]
    fn find_match_basename_and_full_path() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        write_file(root, "sub/foo.rs", "")?;
        write_file(root, "sub/bar.rs", "")?;

        let basename = find(
            &scoped_in(root, root),
            "^foo",
            FindOptions {
                match_basename: true,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            basename.content.contains("foo.rs"),
            "got {}",
            basename.content
        );
        assert!(
            !basename.content.contains("bar.rs"),
            "got {}",
            basename.content
        );

        let fullpath_anchored = find(
            &scoped_in(root, root),
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
            &scoped_in(root, root),
            r"sub.*foo\.rs$",
            FindOptions {
                match_basename: false,
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(
            fullpath_ok.content.contains("foo.rs"),
            "got {}",
            fullpath_ok.content
        );
        Ok(())
    }

    #[test]
    fn find_skips_symlink_pointing_outside() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        let outside = tempfile::TempDir::new()?;
        std::fs::write(outside.path().join("secret_target.txt"), "x")?;
        write_file(root, "inside.txt", "x")?;
        std::os::unix::fs::symlink(
            outside.path().join("secret_target.txt"),
            root.join("link.txt"),
        )?;
        let res = find(
            &scoped_in(root, root),
            ".*\\.txt$",
            FindOptions {
                respect_gitignore: false,
                ..Default::default()
            },
        )?;
        assert!(res.content.contains("inside.txt"), "got {}", res.content);
        assert!(
            !res.content.contains("link.txt"),
            "symlink escape leaked: {}",
            res.content
        );
        assert!(
            !res.content.contains("secret_target"),
            "leaked: {}",
            res.content
        );
        Ok(())
    }

    #[test]
    fn find_respects_max_results_exactly() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let root = td.path();
        for i in 0..20 {
            write_file(root, &format!("f{}.txt", i), "x")?;
        }
        let res = find(
            &scoped_in(root, root),
            r"\.txt$",
            FindOptions {
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
}
