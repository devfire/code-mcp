use crate::error::AppError;
use crate::tools::ToolResponse;
use std::fmt::Write;
use std::path::{Component, Path};

/// Load a memory file from the given directory, or return an index/listing.
///
/// - If `name` is `Some`, reads that specific `.md` file (rejecting path traversal).
/// - If `name` is `None`, returns `MEMORY.md` if present, otherwise a listing of
///   all `.md` files in the directory.
///
/// Returns a [`ToolResponse`] (with no truncation/match metadata, since memory
/// files are read whole) so the caller in `server.rs` can treat this like any
/// other tool entry point.
/// Validate that `name` is a single plain filename with no directory
/// components. Uses `Path::components` rather than substring checks, so
/// legitimate names containing `..` (e.g. `foo..bar.md`) are accepted while
/// traversal (`../x`, `sub/y`, absolute paths, `.`/`..`) is rejected.
/// Backslash is rejected outright: it is a separator on Windows, so allowing
/// it would be a latent cross-platform escape even though it is an ordinary
/// filename character on Unix.
fn validate_memory_name(name: &str) -> Result<(), AppError> {
    let invalid = |why: &str| {
        AppError::InvalidRequest(format!(
            "memory name must be a plain filename ({why}), got: {name:?}"
        ))
    };
    if name.is_empty() {
        return Err(invalid("empty"));
    }
    if name.contains('\\') {
        return Err(invalid("backslash is not allowed"));
    }
    let mut components = Path::new(name).components();
    let first = components.next().ok_or_else(|| invalid("empty"))?;
    // The name must be exactly one `Normal` component — no root, prefix,
    // current-dir, parent-dir, or additional trailing components. Each
    // variant is spelled out (no wildcard) so a future `Component` variant
    // forces a conscious decision here instead of falling through.
    match first {
        Component::Normal(_) => {}
        Component::Prefix(_) => return Err(invalid("must not be a Windows prefix path")),
        Component::RootDir => return Err(invalid("must not be absolute")),
        Component::CurDir => return Err(invalid("must not be '.'")),
        Component::ParentDir => return Err(invalid("must not be '..'")),
    }
    if components.next().is_some() {
        return Err(invalid("must not contain '/'"));
    }
    Ok(())
}

pub fn load_memory(dir: &Path, name: Option<&str>) -> Result<ToolResponse, AppError> {
    if !dir.is_dir() {
        return Err(AppError::NotFound(format!(
            "memory dir does not exist: {}",
            dir.display()
        )));
    }

    if let Some(name) = name {
        validate_memory_name(name)?;
        // Canonical prefix check: `dir.join(name)` alone does not resolve
        // symlinks, so `memory/evil.md -> /etc/passwd` would otherwise pass
        // the component check and leak outside content. Canonicalize both
        // sides and require containment, mirroring `Scope::check`.
        let dir_canon = dir.canonicalize().map_err(|e| {
            AppError::NotFound(format!("memory dir does not exist: {}: {e}", dir.display()))
        })?;
        let joined = dir.join(name);
        let file_canon = joined
            .canonicalize()
            .map_err(|_| AppError::NotFound(format!("memory not found: {name}")))?;
        if !file_canon.starts_with(&dir_canon) {
            return Err(AppError::OutOfScope(format!(
                "{name:?} resolves outside memory dir {}",
                dir_canon.display()
            )));
        }
        if !file_canon.is_file() {
            return Err(AppError::NotFound(format!("memory not found: {name}")));
        }
        return Ok(ToolResponse::text(std::fs::read_to_string(&file_canon)?));
    }

    // No name: prefer MEMORY.md, otherwise list *.md files. The index itself
    // is a fixed filename (no user input), but it could still be a symlink
    // pointing outside the memory dir — resolve and enforce containment
    // rather than leaking outside content.
    let index = dir.join("MEMORY.md");
    if std::fs::symlink_metadata(&index).is_ok() {
        let dir_canon = dir.canonicalize().map_err(AppError::Io)?;
        match index.canonicalize() {
            Ok(index_canon) => {
                if !index_canon.starts_with(&dir_canon) {
                    return Err(AppError::OutOfScope(format!(
                        "MEMORY.md resolves outside memory dir {}",
                        dir_canon.display()
                    )));
                }
                match std::fs::read_to_string(&index_canon) {
                    Ok(content) => return Ok(ToolResponse::text(content)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(AppError::Io(e)),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AppError::Io(e)),
        }
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

    #[test]
    fn load_memory_accepts_dots_inside_filename() -> TestResult {
        // Regression: the old `contains("..")` substring check rejected
        // legitimate names like `foo..bar.md`. Component validation accepts them.
        let td = TempDir::new()?;
        fs::write(td.path().join("foo..bar.md"), "dots ok\n")?;
        let out = load_memory(td.path(), Some("foo..bar.md"))?.content;
        assert_eq!(out, "dots ok\n");
        Ok(())
    }

    #[test]
    fn load_memory_rejects_symlink_pointing_outside() -> TestResult {
        let td = TempDir::new()?;
        let outside = TempDir::new()?;
        fs::write(outside.path().join("secret.txt"), "secret\n")?;
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), td.path().join("evil.md"))?;
        match load_memory(td.path(), Some("evil.md")) {
            Err(AppError::OutOfScope(_)) | Err(AppError::NotFound(_)) => Ok(()),
            other => Err(format!("expected OutOfScope/NotFound, got {:?}", other).into()),
        }
    }

    #[test]
    fn load_memory_index_symlink_outside_is_rejected() -> TestResult {
        let td = TempDir::new()?;
        let outside = TempDir::new()?;
        fs::write(outside.path().join("secret.txt"), "secret\n")?;
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            td.path().join("MEMORY.md"),
        )?;
        match load_memory(td.path(), None) {
            Err(AppError::OutOfScope(_)) => Ok(()),
            other => Err(format!("expected OutOfScope, got {:?}", other).into()),
        }
    }
}
