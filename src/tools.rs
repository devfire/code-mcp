use crate::error::AppError;
use grep_regex::RegexMatcher;
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch, SinkContextKind};
use ignore::WalkBuilder;
use regex::Regex;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub fn grep(
    directory: &str,
    pattern: &str,
    before_context: usize,
    after_context: usize,
    max_results: Option<usize>,
) -> Result<String, AppError> {
    let matcher = RegexMatcher::new(pattern)?;
    let searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .before_context(before_context)
        .after_context(after_context)
        .build();

    let output = Arc::new(Mutex::new(String::new()));
    let result_count = Arc::new(Mutex::new(0usize));
    let max_res = max_results.unwrap_or(100);

    let walker = WalkBuilder::new(directory).build_parallel();

    walker.run(|| {
        let output = output.clone();
        let result_count = result_count.clone();
        let mut local_searcher = searcher.clone();
        let local_matcher = matcher.clone();

        Box::new(move |result| {
            // Check if we reached the maximum results
            if *result_count.lock().unwrap() >= max_res {
                return ignore::WalkState::Quit;
            }

            if let Ok(entry) = result {
                if entry.file_type().map_or(false, |ft| ft.is_file()) {
                    let path = entry.path();
                    
                    struct MySink<'a> {
                        path: &'a std::path::Path,
                        output: &'a Arc<Mutex<String>>,
                        count: &'a Arc<Mutex<usize>>,
                        max_res: usize,
                    }

                    impl<'a> Sink for MySink<'a> {
                        type Error = io::Error;

                        fn matched(&mut self, _searcher: &grep_searcher::Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
                            let mut out = self.output.lock().unwrap();
                            let mut cnt = self.count.lock().unwrap();
                            if *cnt >= self.max_res {
                                return Ok(false);
                            }
                            *cnt += 1;
                            let line_num = mat.line_number().unwrap_or(0);
                            let line = String::from_utf8_lossy(mat.bytes());
                            out.push_str(&format!("{}:{}: {}", self.path.display(), line_num, line));
                            if !line.ends_with('\n') {
                                out.push('\n');
                            }
                            Ok(true)
                        }

                        fn context(&mut self, _searcher: &grep_searcher::Searcher, ctx: &grep_searcher::SinkContext<'_>) -> Result<bool, io::Error> {
                            let mut out = self.output.lock().unwrap();
                            let line_num = ctx.line_number().unwrap_or(0);
                            let line = String::from_utf8_lossy(ctx.bytes());
                            let separator = match ctx.kind() {
                                SinkContextKind::Before => "-",
                                SinkContextKind::After => "-",
                                _ => "-",
                            };
                            out.push_str(&format!("{}{}{} {}", self.path.display(), separator, line_num, line));
                            if !line.ends_with('\n') {
                                out.push('\n');
                            }
                            Ok(true)
                        }
                    }

                    let mut sink = MySink {
                        path,
                        output: &output,
                        count: &result_count,
                        max_res,
                    };

                    let _ = local_searcher.search_path(&local_matcher, path, &mut sink);
                }
            }
            ignore::WalkState::Continue
        })
    });

    let final_output = output.lock().unwrap().clone();
    if final_output.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(final_output)
    }
}

pub fn find(
    directory: &str,
    pattern: &str,
    max_results: Option<usize>,
) -> Result<String, AppError> {
    let re = Regex::new(pattern)?;
    let max_res = max_results.unwrap_or(100);
    
    let output = Arc::new(Mutex::new(String::new()));
    let result_count = Arc::new(Mutex::new(0usize));
    
    let walker = WalkBuilder::new(directory).build_parallel();

    walker.run(|| {
        let output = output.clone();
        let result_count = result_count.clone();
        let re = re.clone();

        Box::new(move |result| {
            if *result_count.lock().unwrap() >= max_res {
                return ignore::WalkState::Quit;
            }

            if let Ok(entry) = result {
                let path_str = entry.path().to_string_lossy();
                if re.is_match(&path_str) {
                    let mut out = output.lock().unwrap();
                    let mut cnt = result_count.lock().unwrap();
                    if *cnt >= max_res {
                        return ignore::WalkState::Quit;
                    }
                    *cnt += 1;
                    out.push_str(&path_str);
                    out.push('\n');
                }
            }
            ignore::WalkState::Continue
        })
    });

    let final_output = output.lock().unwrap().clone();
    if final_output.is_empty() {
        Ok("No matches found.".to_string())
    } else {
        Ok(final_output)
    }
}

pub fn cat(file_path: &str, max_lines: Option<usize>) -> Result<String, AppError> {
    let path = PathBuf::from(file_path);
    if !path.is_file() {
        return Err(AppError::InvalidRequest("Target is not a file or does not exist".to_string()));
    }

    let file = File::open(&path)?;
    let mut reader = BufReader::new(file);

    let max = max_lines.unwrap_or(2000);
    let mut output = String::new();
    let mut line_count = 0;

    let mut buf = String::new();
    while reader.read_line(&mut buf)? > 0 {
        if line_count >= max {
            output.push_str("\n... [Output truncated due to limit] ...\n");
            break;
        }
        output.push_str(&buf);
        buf.clear();
        line_count += 1;
    }

    Ok(output)
}
