use crate::error::AppError;
use crate::tools::ToolResponse;
use std::fmt::Write;
use std::path::Path;

/// Load a memory file from the given directory, or return an index/listing.
///
/// - If `name` is `Some`, reads that specific `.md` file (rejecting path traversal).
/// - If `name` is `None`, returns `MEMORY.md` if present, otherwise a listing of
///   all `.md` files in the directory.
///
/// Returns a [`ToolResponse`] (with no truncation/match metadata, since memory
/// files are read whole) so the caller in `server.rs` can treat this like any
/// other tool entry point.
pub fn load_memory(dir: &Path, name: Option<&str>) -> Result<ToolResponse, AppError> {
    if !dir.is_dir() {
        return Err(AppError::NotFound(format!(
            "memory dir does not exist: {}",
            dir.display()
        )));
    }

    if let Some(name) = name {
        // Reject path traversal: name must be a single, non-empty path component.
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(AppError::InvalidRequest(format!(
                "memory name must be a plain filename, got: {name:?}"
            )));
        }
        let path = dir.join(name);
        if !path.is_file() {
            return Err(AppError::NotFound(format!("memory not found: {name}")));
        }
        return Ok(ToolResponse::text(std::fs::read_to_string(&path)?));
    }

    // No name: prefer MEMORY.md, otherwise list *.md files.
    let index = dir.join("MEMORY.md");
    if index.is_file() {
        return Ok(ToolResponse::text(std::fs::read_to_string(&index)?));
    }

    let mut listing = String::from("# Memory dir contents\n\n");
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == "md")
        })
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    if entries.is_empty() {
        listing.push_str("(no .md files found; configure MEMORY.md or add memory files)\n");
    } else {
        for e in entries {
            if let Some(name) = e.file_name().to_str() {
                let _ = writeln!(listing, "- {name}");
            }
        }
        listing.push_str(
            "\nUse `memories(name=\"...\")` to load a specific memory, \
or create a `MEMORY.md` index at the top level.\n",
        );
    }
    Ok(ToolResponse::text(listing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn load_memory_returns_index_when_memory_md_present() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("MEMORY.md"), "# index\n- foo\n")?;
        fs::write(td.path().join("foo.md"), "ignored\n")?;

        let out = load_memory(td.path(), None)?.content;
        assert!(out.starts_with("# index"), "got {:?}", out);
        Ok(())
    }

    #[test]
    fn load_memory_lists_md_files_when_no_index() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("a.md"), "")?;
        fs::write(td.path().join("b.md"), "")?;
        fs::write(td.path().join("ignore.txt"), "")?;

        let out = load_memory(td.path(), None)?.content;
        assert!(out.contains("- a.md"), "got {}", out);
        assert!(out.contains("- b.md"), "got {}", out);
        assert!(!out.contains("ignore.txt"), "got {}", out);
        Ok(())
    }

    #[test]
    fn load_memory_returns_named_file() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("user_role.md"), "data scientist\n")?;

        let out = load_memory(td.path(), Some("user_role.md"))?.content;
        assert_eq!(out, "data scientist\n");
        Ok(())
    }

    #[test]
    fn load_memory_rejects_path_traversal() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("ok.md"), "ok\n")?;

        for bad in ["../etc/passwd", "sub/foo.md", "..\\foo", "..", ""] {
            match load_memory(td.path(), Some(bad)) {
                Err(AppError::InvalidRequest(_)) => {}
                Err(AppError::NotFound(_)) if bad.is_empty() => {}
                other => {
                    return Err(format!("expected rejection for {:?}, got {:?}", bad, other).into());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn load_memory_errors_on_missing_dir() -> TestResult {
        let td = TempDir::new()?;
        let missing = td.path().join("nope");
        match load_memory(&missing, None) {
            Err(AppError::NotFound(_)) => Ok(()),
            other => Err(format!("expected NotFound, got {:?}", other).into()),
        }
    }
}
