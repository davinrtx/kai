//! Bounded windowed file and stream reader.
//!
//! Enforces bounded, streaming reads with explicit 1-based line bounds (`offset` and `limit`)
//! without loading entire files into memory. Includes strict per-line byte caps (64 KB)
//! to defend against out-of-memory denial-of-service from minified or binary files.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use kai_core::{ContextError, InternalError, KaiError, Result};

/// Maximum bytes retained per line to prevent unbounded heap allocations on minified files.
pub const MAX_LINE_BUFFER_BYTES: usize = 65536;

/// Output of a bounded window read operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowResult {
    /// 1-based start line number of the returned window.
    pub start_line: usize,
    /// 1-based end line number of the returned window (inclusive).
    pub end_line: usize,
    /// Total number of lines in the source document.
    pub total_lines: usize,
    /// Extracted lines joined by newline characters.
    pub content: String,
}

impl WindowResult {
    /// Formats lines with explicit 1-based line number prefixes (e.g., `   1 | line content`).
    pub fn format_with_line_numbers(&self) -> String {
        if self.content.is_empty() {
            return String::new();
        }

        let width = format!("{}", self.end_line).len().max(3);
        let mut formatted =
            String::with_capacity(self.content.len() + 16 * (self.end_line - self.start_line + 1));

        for (idx, line) in self.content.lines().enumerate() {
            let line_num = self.start_line + idx;
            if idx > 0 {
                formatted.push('\n');
            }
            formatted.push_str(&format!("{:>width$} | {}", line_num, line, width = width));
        }

        formatted
    }
}

/// Bounded window reader utility.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowReader;

impl WindowReader {
    /// Reads a bounded line window from a file on disk.
    ///
    /// Streams through the file line by line without allocating the entire file into memory.
    /// Defends against unbounded lines by capping each line buffer at 64 KB.
    ///
    /// # Errors
    /// Returns [`ContextError::InvalidWindow`] if `offset == 0`, `limit == 0`, or `offset > total_lines`.
    /// Returns [`InternalError`] if the file cannot be opened or read.
    pub fn read_lines<P: AsRef<Path>>(
        path: P,
        offset: usize,
        limit: usize,
    ) -> Result<WindowResult> {
        let path_ref = path.as_ref();
        if path_ref.is_dir() {
            return Err(KaiError::Context(ContextError::InvalidWindow {
                offset,
                limit,
            }));
        }
        if offset == 0 || limit == 0 {
            return Err(KaiError::Context(ContextError::InvalidWindow {
                offset,
                limit,
            }));
        }

        let file = File::open(path_ref).map_err(|err| {
            KaiError::Internal(InternalError::with_cause(
                format!("Failed to open file '{}'", path_ref.display()),
                err,
            ))
        })?;

        let reader = BufReader::new(file);
        Self::read_from_reader(reader, offset, limit, Some(path_ref.display().to_string()))
    }

    /// Reads a bounded line window from an in-memory string slice.
    ///
    /// # Errors
    /// Returns [`ContextError::InvalidWindow`] if `offset == 0`, `limit == 0`, or `offset > total_lines`.
    pub fn read_str(content: &str, offset: usize, limit: usize) -> Result<WindowResult> {
        if offset == 0 || limit == 0 {
            return Err(KaiError::Context(ContextError::InvalidWindow {
                offset,
                limit,
            }));
        }

        let reader = std::io::Cursor::new(content.as_bytes());
        Self::read_from_reader(reader, offset, limit, None)
    }

    /// Reads lines surrounding a target line with padding lines before and after.
    ///
    /// Ideal for inspecting grep matches or compiler diagnostic locations with neighborhood context.
    ///
    /// # Errors
    /// Returns [`ContextError::InvalidWindow`] if `target_line == 0`.
    pub fn read_context<P: AsRef<Path>>(
        path: P,
        target_line: usize,
        padding: usize,
    ) -> Result<WindowResult> {
        if target_line == 0 {
            return Err(KaiError::Context(ContextError::InvalidWindow {
                offset: 0,
                limit: padding * 2 + 1,
            }));
        }

        let offset = target_line.saturating_sub(padding).max(1);
        let limit = target_line.saturating_add(padding).saturating_sub(offset) + 1;
        Self::read_lines(path, offset, limit)
    }

    /// Internal streaming reader consuming any [`BufRead`] implementor with bounded per-line reads.
    fn read_from_reader<R: BufRead>(
        mut reader: R,
        offset: usize,
        limit: usize,
        source_label: Option<String>,
    ) -> Result<WindowResult> {
        let mut total_lines = 0usize;
        let mut window_lines = Vec::with_capacity(limit.min(256));
        let end_target = offset.saturating_add(limit).saturating_sub(1);
        let mut line_buf = String::with_capacity(1024);

        loop {
            let label = source_label.as_deref().unwrap_or("buffered stream");
            let read_opt =
                Self::read_bounded_line(&mut reader, &mut line_buf, MAX_LINE_BUFFER_BYTES)
                    .map_err(|err| {
                        KaiError::Internal(InternalError::with_cause(
                            format!("Error reading line {} from {}", total_lines + 1, label),
                            err,
                        ))
                    })?;

            let Some(truncated) = read_opt else {
                break; // EOF
            };

            total_lines += 1;

            if total_lines >= offset && total_lines <= end_target {
                if truncated {
                    let mut bounded = line_buf.clone();
                    bounded.push_str(" [Line truncated: exceeded 64KB cap]");
                    window_lines.push(bounded);
                } else {
                    window_lines.push(line_buf.clone());
                }
            }

            if total_lines >= end_target {
                // All requested window lines collected. Transition to fast raw-byte scanning
                // to count remaining lines without allocations or UTF-8 decoding.
                let mut last_byte = b'\n';
                loop {
                    let available = reader.fill_buf().map_err(|err| {
                        KaiError::Internal(InternalError::with_cause(
                            format!("Error fast-counting lines from {}", label),
                            err,
                        ))
                    })?;
                    if available.is_empty() {
                        break;
                    }
                    total_lines += available.iter().filter(|&&b| b == b'\n').count();
                    last_byte = *available.last().unwrap_or(&b'\n');
                    let len = available.len();
                    reader.consume(len);
                }
                if last_byte != b'\n' {
                    total_lines += 1;
                }
                break;
            }
        }

        // Handle empty file edge case: offset 1 is valid, returns 0 lines
        if total_lines == 0 {
            if offset == 1 {
                return Ok(WindowResult {
                    start_line: 1,
                    end_line: 0,
                    total_lines: 0,
                    content: String::new(),
                });
            } else {
                return Err(KaiError::Context(ContextError::InvalidWindow {
                    offset,
                    limit,
                }));
            }
        }

        if offset > total_lines {
            return Err(KaiError::Context(ContextError::InvalidWindow {
                offset,
                limit,
            }));
        }

        let end_line = offset + window_lines.len().saturating_sub(1);
        let content = window_lines.join("\n");

        Ok(WindowResult {
            start_line: offset,
            end_line,
            total_lines,
            content,
        })
    }

    /// Reads a single line up to `max_bytes`, advancing past newline without unbounded allocations.
    ///
    /// Returns `Ok(Some(was_truncated))` if a line was read, or `Ok(None)` on EOF.
    pub fn read_bounded_line<R: BufRead>(
        reader: &mut R,
        buf: &mut String,
        max_bytes: usize,
    ) -> std::io::Result<Option<bool>> {
        buf.clear();
        let mut total_line_bytes = 0usize;
        let mut was_truncated = false;

        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                if total_line_bytes == 0 {
                    return Ok(None);
                } else {
                    return Ok(Some(was_truncated));
                }
            }

            if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                let chunk_len = pos + 1;
                let to_save = &available[..pos]; // omit trailing \n
                let clean_to_save = if to_save.ends_with(b"\r") {
                    &to_save[..to_save.len().saturating_sub(1)]
                } else {
                    to_save
                };

                if total_line_bytes < max_bytes {
                    let allowed = (max_bytes - total_line_bytes).min(clean_to_save.len());
                    buf.push_str(&String::from_utf8_lossy(&clean_to_save[..allowed]));
                    if allowed < clean_to_save.len() {
                        was_truncated = true;
                    }
                } else {
                    was_truncated = true;
                }

                reader.consume(chunk_len);
                return Ok(Some(was_truncated));
            } else {
                let chunk_len = available.len();
                if total_line_bytes < max_bytes {
                    let allowed = (max_bytes - total_line_bytes).min(chunk_len);
                    buf.push_str(&String::from_utf8_lossy(&available[..allowed]));
                    if allowed < chunk_len {
                        was_truncated = true;
                    }
                } else {
                    was_truncated = true;
                }
                total_line_bytes += chunk_len;
                reader.consume(chunk_len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_read_str_window_middle() {
        let text = "alpha\nbeta\ngamma\ndelta\nepsilon";
        let result = WindowReader::read_str(text, 2, 2).expect("must read window");
        assert_eq!(result.start_line, 2);
        assert_eq!(result.end_line, 3);
        assert_eq!(result.total_lines, 5);
        assert_eq!(result.content, "beta\ngamma");
    }

    #[test]
    fn test_read_str_window_overflow_limit() {
        let text = "line 1\nline 2\nline 3";
        let result = WindowReader::read_str(text, 2, 10).expect("must read window");
        assert_eq!(result.start_line, 2);
        assert_eq!(result.end_line, 3);
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.content, "line 2\nline 3");
    }

    #[test]
    fn test_invalid_bounds() {
        let text = "a\nb\nc";
        assert!(matches!(
            WindowReader::read_str(text, 0, 2),
            Err(KaiError::Context(ContextError::InvalidWindow {
                offset: 0,
                limit: 2
            }))
        ));
        assert!(matches!(
            WindowReader::read_str(text, 1, 0),
            Err(KaiError::Context(ContextError::InvalidWindow {
                offset: 1,
                limit: 0
            }))
        ));
        assert!(matches!(
            WindowReader::read_str(text, 5, 1),
            Err(KaiError::Context(ContextError::InvalidWindow {
                offset: 5,
                limit: 1
            }))
        ));
    }

    #[test]
    fn test_format_with_line_numbers() {
        let text = "first\nsecond\nthird";
        let result = WindowReader::read_str(text, 1, 3).expect("must read");
        let formatted = result.format_with_line_numbers();
        assert!(formatted.contains("  1 | first"));
        assert!(formatted.contains("  2 | second"));
        assert!(formatted.contains("  3 | third"));
    }

    #[test]
    fn test_read_file_streaming() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("kai_test_window.txt");
        {
            let mut file = File::create(&file_path).expect("create temp file");
            for i in 1..=50 {
                writeln!(file, "row {}", i).expect("write row");
            }
        }

        let result = WindowReader::read_lines(&file_path, 10, 5).expect("must read lines");
        assert_eq!(result.start_line, 10);
        assert_eq!(result.end_line, 14);
        assert_eq!(result.total_lines, 50);
        assert_eq!(result.content, "row 10\nrow 11\nrow 12\nrow 13\nrow 14");

        let _ = std::fs::remove_file(file_path);
    }

    #[test]
    fn test_bounded_line_reader_defends_against_massive_lines() {
        // Create 200 KB single-line string
        let massive_line = "A".repeat(200_000);
        let mut reader = std::io::Cursor::new(massive_line.as_bytes());
        let mut buf = String::new();
        let read_res = WindowReader::read_bounded_line(&mut reader, &mut buf, 1024)
            .expect("must read bounded");

        assert_eq!(read_res, Some(true));
        assert_eq!(buf.len(), 1024);
    }

    #[test]
    fn test_read_context() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("kai_test_window_ctx.txt");
        {
            let mut file = File::create(&file_path).expect("create temp file");
            for i in 1..=30 {
                writeln!(file, "line {}", i).expect("write line");
            }
        }

        let result = WindowReader::read_context(&file_path, 15, 2).expect("must read context");
        assert_eq!(result.start_line, 13);
        assert_eq!(result.end_line, 17);
        assert_eq!(result.total_lines, 30);
        assert_eq!(
            result.content,
            "line 13\nline 14\nline 15\nline 16\nline 17"
        );

        let _ = std::fs::remove_file(file_path);
    }

    #[test]
    fn test_is_dir_fails_gracefully() {
        let temp_dir = std::env::temp_dir();
        let res = WindowReader::read_lines(&temp_dir, 1, 10);
        assert!(matches!(
            res,
            Err(KaiError::Context(ContextError::InvalidWindow { .. }))
        ));
    }

    #[test]
    fn test_fast_line_count_large_stream() {
        // Build 5,000 lines: 2,500 with newline, trailing without newline
        let mut data = String::new();
        for i in 1..=5000 {
            if i < 5000 {
                data.push_str(&format!("row {}\n", i));
            } else {
                data.push_str(&format!("row {}", i)); // no trailing newline on last line
            }
        }

        // Read window of lines 5..10 (limit 6)
        let res = WindowReader::read_str(&data, 5, 6).expect("read str window");
        assert_eq!(res.start_line, 5);
        assert_eq!(res.end_line, 10);
        assert_eq!(res.total_lines, 5000);
        assert_eq!(res.content, "row 5\nrow 6\nrow 7\nrow 8\nrow 9\nrow 10");
    }
}
