use crate::error::AppError;
use std::path::{Path, PathBuf};

/// A filesystem scope. When a `root` is configured, every path the tools
/// touch is canonicalized and required to lie under that root. With no
/// root configured the scope is a no-op (any path is allowed).
#[derive(Debug, Clone)]
pub struct Scope {
    root: Option<PathBuf>,
}

impl Scope {
    /// Build a scope. The root, if given, must exist and be a directory.
    /// It is canonicalized at construction time so symlinks inside the
    /// configured path are resolved once.
    pub fn new(root: Option<PathBuf>) -> Result<Self, AppError> {
        match root {
            None => Ok(Self { root: None }),
            Some(p) => {
                let canon = p.canonicalize().map_err(|e| {
                    AppError::Internal(format!(
                        "--project {}: {}",
                        p.display(),
                        e
                    ))
                })?;
                if !canon.is_dir() {
                    return Err(AppError::Internal(format!(
                        "--project must be a directory: {}",
                        canon.display()
                    )));
                }
                Ok(Self { root: Some(canon) })
            }
        }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Validate that `input` is within the scope, returning its canonical
    /// path. If no root is configured, `input` is returned unchanged.
    /// Symlinks in `input` are resolved before the containment check, so
    /// a symlink inside the project that points outside it is rejected.
    pub fn check<P: AsRef<Path>>(&self, input: P) -> Result<PathBuf, AppError> {
        let input = input.as_ref();
        let Some(root) = &self.root else {
            return Ok(input.to_path_buf());
        };
        let canon = input.canonicalize().map_err(|e| {
            AppError::NotFound(format!("{}: {}", input.display(), e))
        })?;
        if !canon.starts_with(root) {
            return Err(AppError::OutOfScope(format!(
                "{} is outside project root {}",
                canon.display(),
                root.display()
            )));
        }
        Ok(canon)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn no_root_is_a_no_op() -> TestResult {
        let s = Scope::new(None)?;
        let p = s.check("/etc/passwd")?;
        assert_eq!(p, PathBuf::from("/etc/passwd"));
        Ok(())
    }

    #[test]
    fn check_accepts_path_inside_root() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("a.txt"), "x")?;
        let s = Scope::new(Some(td.path().to_path_buf()))?;
        let canon = s.check(td.path().join("a.txt"))?;
        assert!(canon.ends_with("a.txt"), "got {}", canon.display());
        Ok(())
    }

    #[test]
    fn check_rejects_path_outside_root() -> TestResult {
        let td = TempDir::new()?;
        let s = Scope::new(Some(td.path().to_path_buf()))?;
        match s.check("/etc/passwd") {
            Err(AppError::OutOfScope(_)) => Ok(()),
            other => Err(format!("expected OutOfScope, got {:?}", other).into()),
        }
    }

    #[test]
    fn check_resolves_symlink_to_outside_root() -> TestResult {
        // Place a symlink inside the project that points outside it.
        let td = TempDir::new()?;
        let outside = TempDir::new()?;
        let target = outside.path().join("secret.txt");
        fs::write(&target, "secret\n")?;

        let link = td.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link)?;

        let s = Scope::new(Some(td.path().to_path_buf()))?;
        match s.check(&link) {
            Err(AppError::OutOfScope(_)) => Ok(()),
            other => Err(format!("expected OutOfScope, got {:?}", other).into()),
        }
    }

    #[test]
    fn check_rejects_path_traversal() -> TestResult {
        // Even ../.. style paths get canonicalized before the prefix check.
        let td = TempDir::new()?;
        let s = Scope::new(Some(td.path().to_path_buf()))?;
        let traversal = td.path().join("..").join("..");
        match s.check(&traversal) {
            // canonicalize-resolves to a directory that is not under root.
            Err(AppError::OutOfScope(_)) => Ok(()),
            // ...or, if the parent happens to be inside, we just fail to escape.
            // Either way, reading /etc/passwd via traversal should not succeed.
            Err(AppError::NotFound(_)) => Ok(()),
            Ok(p) => {
                if p.starts_with(td.path()) {
                    Ok(())
                } else {
                    Err(format!("escaped root: {}", p.display()).into())
                }
            }
            other => Err(format!("unexpected: {:?}", other).into()),
        }
    }

    #[test]
    fn root_must_exist() -> TestResult {
        match Scope::new(Some(PathBuf::from("/nonexistent/path/here"))) {
            Err(AppError::Internal(_)) => Ok(()),
            other => Err(format!("expected Internal, got {:?}", other).into()),
        }
    }

    #[test]
    fn root_must_be_a_directory() -> TestResult {
        let td = TempDir::new()?;
        let f = td.path().join("not_a_dir");
        fs::write(&f, "")?;
        match Scope::new(Some(f)) {
            Err(AppError::Internal(_)) => Ok(()),
            other => Err(format!("expected Internal, got {:?}", other).into()),
        }
    }
}
