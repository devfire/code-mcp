//! Shared helpers for the walker-based tools: parallel walker construction,
//! extension filtering, error capture, and byte-capped channel draining.

use ignore::WalkBuilder;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Receiver;

/// Trait for option structs that can configure a parallel directory walker.
///
/// Both [`super::options::GrepOptions`] and [`super::options::FindOptions`]
/// implement this so [`build_parallel_walker`] is reusable across tools.
pub(crate) trait WalkerConfig {
    fn include_hidden(&self) -> bool;
    fn respect_gitignore(&self) -> bool;
    fn follow_symlinks(&self) -> bool;
}

/// Record the first error message into `slot`, ignoring later ones.
pub(crate) fn record_first(slot: &Mutex<Option<String>>, msg: String) {
    if let Ok(mut guard) = slot.lock()
        && guard.is_none()
    {
        *guard = Some(msg);
    }
}

/// Build a parallel walker from any option struct implementing [`WalkerConfig`].
pub(crate) fn build_parallel_walker(
    directory: &str,
    opts: &impl WalkerConfig,
) -> ignore::WalkParallel {
    WalkBuilder::new(directory)
        .hidden(!opts.include_hidden())
        .git_ignore(opts.respect_gitignore())
        .git_global(opts.respect_gitignore())
        .git_exclude(opts.respect_gitignore())
        .follow_links(opts.follow_symlinks())
        .build_parallel()
}

/// Check whether a directory entry's extension matches the filter list.
/// Returns `true` if the file should be searched.
pub(crate) fn extension_matches(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.iter().any(|w| w == e))
}

/// Drain string chunks from `rx` into a single output buffer, enforcing the
/// authoritative `max_bytes` cap. When the cap is hit the final chunk is cut on
/// a UTF-8 character boundary and a trailing newline is ensured; remaining
/// chunks are discarded. Returns the assembled output and whether the cap was
/// hit (`true` => truncated).
pub(crate) fn drain_capped(rx: &Receiver<String>, max_bytes: usize) -> (String, bool) {
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
    (output, byte_cap_hit)
}
