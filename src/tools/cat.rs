//! The `cat` tool: read file contents with line/byte pagination.

use super::response::{ToolResponse, TruncationReason};
use crate::error::AppError;
use crate::scope::ScopedPath;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::num::NonZeroUsize;

/// Read file contents with pagination.
///
/// Skips `offset` lines (0-based), then reads up to `max_lines` lines or
/// `max_bytes` bytes, whichever is hit first. Byte-cap cuts are performed on
/// UTF-8 character boundaries so the output is always valid UTF-8. Truncation
/// is reported via the returned [`ToolResponse`]'s `truncated` /
/// `truncation_reason` fields (`line_cap` or `byte_cap`).
///
/// Returns [`AppError::InvalidRequest`] if the target is missing or not a
/// regular file.
pub fn cat(
    file_path: &ScopedPath,
    offset: usize,
    max_lines: NonZeroUsize,
    max_bytes: NonZeroUsize,
) -> Result<ToolResponse, AppError> {
    let (max_lines, max_bytes) = (max_lines.get(), max_bytes.get());
    if !file_path.as_ref().is_file() {
        return Err(AppError::InvalidRequest(
            "Target is not a file or does not exist".to_string(),
        ));
    }

    let file = File::open(file_path.as_ref())?;

    let mut reader = BufReader::new(file);

    // Skip `offset` lines.
    let mut skip_buf = String::new();
    for _ in 0..offset {
        skip_buf.clear();
        let n = reader.read_line(&mut skip_buf)?;
        if n == 0 {
            return Ok(ToolResponse::text(String::new()));
        }
    }

    let mut output = String::new();
    let mut line_count = 0usize;
    let mut truncated = false;
    let mut truncation_reason: Option<TruncationReason> = None;
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
            truncation_reason = Some(TruncationReason::LineCap);
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
            truncation_reason = Some(TruncationReason::ByteCap);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::{TestResult, nz, scoped_in};
    use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
    use std::fs;

    #[test]
    fn cat_offset_and_line_window() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let path = td.path().join("a.txt");
        fs::write(&path, "L1\nL2\nL3\nL4\nL5\nL6\nL7\n")?;

        let res = cat(&scoped_in(td.path(), &path), 2, nz(3), DEFAULT_MAX_BYTES)?;
        assert!(
            res.content.starts_with("L3\nL4\nL5\n"),
            "got {:?}",
            res.content
        );
        assert!(res.truncated, "expected truncated=true");
        assert_eq!(res.truncation_reason, Some(TruncationReason::LineCap));

        let res = cat(&scoped_in(td.path(), &path), 4, nz(3), DEFAULT_MAX_BYTES)?;
        assert_eq!(res.content, "L5\nL6\nL7\n", "got {:?}", res.content);
        assert!(!res.truncated);
        Ok(())
    }

    #[test]
    fn cat_byte_cap_truncates_with_marker() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let path = td.path().join("a.txt");
        let body = "abcdefghijklmnopqrstuvwxyz\n".repeat(20);
        fs::write(&path, &body)?;

        let res = cat(&scoped_in(td.path(), &path), 0, DEFAULT_MAX_LINES, nz(50))?;
        assert!(res.truncated, "expected truncated=true, got {:?}", res);
        assert_eq!(res.truncation_reason, Some(TruncationReason::ByteCap));
        assert!(
            res.content.len() < body.len(),
            "expected truncation, got len {}",
            res.content.len()
        );
        Ok(())
    }

    #[test]
    fn cat_errors_when_path_is_directory() -> TestResult {
        let td = tempfile::TempDir::new()?;
        match cat(
            &scoped_in(td.path(), td.path()),
            0,
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES,
        ) {
            Err(AppError::InvalidRequest(_)) => Ok(()),
            Err(other) => Err(format!("expected InvalidRequest, got {:?}", other).into()),
            Ok(s) => Err(format!("expected error, got Ok({:?})", s).into()),
        }
    }
}
