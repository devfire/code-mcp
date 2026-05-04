use crate::error::AppError;
use std::path::{Path, PathBuf};

/// A filesystem scope. Every path the tools touch is canonicalized and
/// required to lie under `root`.
#[derive(Debug, Clone)]
pub struct Scope {
    root: PathBuf,
}

impl Scope {
    /// Build a scope. The root must exist and be a directory. It is
    /// canonicalized at construction time so symlinks inside the
    /// configured path are resolved once.
    pub fn new(root: PathBuf) -> Result<Self, AppError> {
        let canon = root.canonicalize().map_err(|e| {
            AppError::Internal(format!("--project {}: {}", root.display(), e))
        })?;
        if !canon.is_dir() {
            return Err(AppError::Internal(format!(
                "--project must be a directory: {}",
                canon.display()
            )));
        }
        Ok(Self { root: canon })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate that `input` is within the scope, returning its canonical
    /// path. Symlinks in `input` are resolved before the containment
    /// check, so a symlink inside the project that points outside it is
    /// rejected.
    pub fn check<P: AsRef<Path>>(&self, input: P) -> Result<PathBuf, AppError> {
        let input = input.as_ref();
        let canon = input.canonicalize().map_err(|e| {
            AppError::NotFound(format!("{}: {}", input.display(), e))
        })?;
        if !canon.starts_with(&self.root) {
            return Err(AppError::OutOfScope(format!(
                "{} is outside project root {}",
                canon.display(),
                self.root.display()
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
    fn check_accepts_path_inside_root() -> TestResult {
        let td = TempDir::new()?;
        fs::write(td.path().join("a.txt"), "x")?;
        let s = Scope::new(td.path().to_path_buf())?;
        let canon = s.check(td.path().join("a.txt"))?;
        assert!(canon.ends_with("a.txt"), "got {}", canon.display());
        Ok(())
    }

    #[test]
    fn check_rejects_path_outside_root() -> TestResult {
        let td = TempDir::new()?;
        let s = Scope::new(td.path().to_path_buf())?;
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

        let s = Scope::new(td.path().to_path_buf())?;
        match s.check(&link) {
            Err(AppError::OutOfScope(_)) => Ok(()),
            other => Err(format!("expected OutOfScope, got {:?}", other).into()),
        }
    }

    #[test]
    fn check_rejects_path_traversal() -> TestResult {
        // Even ../.. style paths get canonicalized before the prefix check.
        let td = TempDir::new()?;
        let s = Scope::new(td.path().to_path_buf())?;
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
        match Scope::new(PathBuf::from("/nonexistent/path/here")) {
            Err(AppError::Internal(_)) => Ok(()),
            other => Err(format!("expected Internal, got {:?}", other).into()),
        }
    }

    #[test]
    fn root_must_be_a_directory() -> TestResult {
        let td = TempDir::new()?;
        let f = td.path().join("not_a_dir");
        fs::write(&f, "")?;
        match Scope::new(f) {
            Err(AppError::Internal(_)) => Ok(()),
            other => Err(format!("expected Internal, got {:?}", other).into()),
        }
    }
}
