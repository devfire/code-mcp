//! The `cat` tool: read file contents with line/byte pagination.

use super::response::ToolResponse;
use crate::error::AppError;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

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
    file_path: &str,
    offset: usize,
    max_lines: usize,
    max_bytes: usize,
) -> Result<ToolResponse, AppError> {
    let path = PathBuf::from(file_path);
    if !path.is_file() {
        return Err(AppError::InvalidRequest(
            "Target is not a file or does not exist".to_string(),
        ));
    }

    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);

    // Skip `offset` lines.
    let mut skip_buf = String::new();
    for _ in 0..offset {
        skip_buf.clear();
        let n = reader.read_line(&mut skip_buf)?;
        if n == 0 {
            // EOF before reaching the offset — nothing to return.
            return Ok(ToolResponse {
                content: String::new(),
                truncated: false,
                truncation_reason: None,
                match_count: None,
                entry_error_count: None,
                search_error_count: None,
                first_error: None,
            });
        }
    }

    let mut output = String::new();
    let mut line_count = 0usize;
    let mut truncated = false;
    let mut truncation_reason: Option<String> = None;
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
            truncation_reason = Some("line_cap".to_string());
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
            truncation_reason = Some("byte_cap".to_string());
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
    use crate::tools::testutil::{TestResult, path_str};
    use crate::tools::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
    use std::fs;

    #[test]
    fn cat_offset_and_line_window() -> TestResult {
        let td = tempfile::TempDir::new()?;
        let path = td.path().join("a.txt");
        fs::write(&path, "L1\nL2\nL3\nL4\nL5\nL6\nL7\n")?;

        let res = cat(path_str(&path)?, 2, 3, DEFAULT_MAX_BYTES)?;
        assert!(
            res.content.starts_with("L3\nL4\nL5\n"),
            "got {:?}",
            res.content
        );
        assert!(res.truncated, "expected truncated=true");
        assert_eq!(res.truncation_reason, Some("line_cap".to_string()));

        let res = cat(path_str(&path)?, 4, 3, DEFAULT_MAX_BYTES)?;
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

        let res = cat(path_str(&path)?, 0, DEFAULT_MAX_LINES, 50)?;
        assert!(res.truncated, "expected truncated=true, got {:?}", res);
        assert_eq!(res.truncation_reason, Some("byte_cap".to_string()));
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
            path_str(td.path())?,
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
