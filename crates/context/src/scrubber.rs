//! Terminal output scrubber and formatter.
//!
//! Provides deterministic ANSI escape code stripping, carriage return normalization,
//! exit-code header injection, and bounded head/tail line preservation for command execution outputs.

use kai_core::MAX_TOOL_OUTPUT_BYTES;
use regex::Regex;
use std::sync::OnceLock;

/// Global cached regular expression for terminal ANSI and OSC escape sequences.
fn ansi_regex() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)\x1B(?:\][^\x07\x1B]*(?:\x07|\x1B\\)|\[[0-?]*[ -/]*[@-~]|[\(\)][a-zA-Z0-9]|[@-Z\\-_])",
        )
        .ok()
    })
    .as_ref()
}

/// Utility for sanitizing and formatting raw terminal output streams.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalScrubber;

impl TerminalScrubber {
    /// Strips ANSI escape sequences and normalizes line endings from raw terminal output.
    ///
    /// Fast-paths strings without ANSI escape codes or carriage returns with zero regex evaluations.
    pub fn clean(raw: &str) -> String {
        if !raw.contains('\x1B') && !raw.contains('\r') {
            return raw.to_string();
        }

        let cleaned = if raw.contains('\x1B') {
            match ansi_regex() {
                Some(re) => re.replace_all(raw, "").into_owned(),
                None => raw.to_string(),
            }
        } else {
            raw.to_string()
        };

        if !cleaned.contains('\r') {
            return cleaned;
        }

        // Normalize CRLF to LF and standalone CR to LF
        let mut result = String::with_capacity(cleaned.len());
        let mut chars = cleaned.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\r' {
                if chars.peek() == Some(&'\n') {
                    // skip '\r', '\n' will be appended next
                    continue;
                }
                result.push('\n');
            } else {
                result.push(ch);
            }
        }
        result
    }

    /// Formats command execution output with an optional exit-code header,
    /// bounded head/tail line preservation, and strict 4 KB byte caps.
    pub fn format_output(
        raw: &str,
        exit_code: Option<i32>,
        max_head_lines: usize,
        max_tail_lines: usize,
    ) -> String {
        let cleaned = Self::clean(raw);
        let lines: Vec<&str> = cleaned.lines().collect();
        let total_lines = lines.len();

        let mut formatted = String::new();

        if let Some(code) = exit_code {
            formatted.push_str(&format!("[Exit Code: {}]\n", code));
        }

        if total_lines <= max_head_lines + max_tail_lines {
            for (idx, line) in lines.iter().enumerate() {
                formatted.push_str(line);
                if idx + 1 < total_lines {
                    formatted.push('\n');
                }
            }
        } else {
            // Include head lines
            for line in &lines[..max_head_lines] {
                formatted.push_str(line);
                formatted.push('\n');
            }

            let omitted = total_lines - max_head_lines - max_tail_lines;
            formatted.push_str(&format!("[... Omitted {} lines ...]\n", omitted));

            // Include tail lines
            let tail_start = total_lines - max_tail_lines;
            for (idx, line) in lines[tail_start..].iter().enumerate() {
                formatted.push_str(line);
                if idx + 1 < max_tail_lines {
                    formatted.push('\n');
                }
            }
        }

        // Apply balanced head/tail truncation if output exceeds 4 KB
        if formatted.len() > MAX_TOOL_OUTPUT_BYTES {
            Self::truncate_head_tail(&formatted, MAX_TOOL_OUTPUT_BYTES)
        } else {
            formatted
        }
    }

    /// Truncates text exceeding `max_bytes` while preserving both head and tail sections.
    pub fn truncate_head_tail(text: &str, max_bytes: usize) -> String {
        if text.len() <= max_bytes {
            return text.to_string();
        }

        let notice = "\n[... Omitted output exceeding byte cap ...]\n";
        if max_bytes <= notice.len() {
            let end = Self::floor_char_boundary(text, max_bytes);
            return text[..end].to_string();
        }

        let budget = max_bytes - notice.len();
        let head_budget = budget / 2;
        let tail_budget = budget - head_budget;

        let head_end = Self::floor_char_boundary(text, head_budget);
        let tail_start = Self::ceil_char_boundary(text, text.len().saturating_sub(tail_budget));

        let mut out = String::with_capacity(max_bytes);
        out.push_str(&text[..head_end]);
        out.push_str(notice);
        out.push_str(&text[tail_start..]);
        out
    }

    /// Finds the largest valid char boundary index <= `index` (MSRV 1.80 compatible).
    fn floor_char_boundary(s: &str, mut index: usize) -> usize {
        if index >= s.len() {
            return s.len();
        }
        while !s.is_char_boundary(index) {
            index = index.saturating_sub(1);
        }
        index
    }

    /// Finds the smallest valid char boundary index >= `index` (MSRV 1.80 compatible).
    fn ceil_char_boundary(s: &str, mut index: usize) -> usize {
        if index >= s.len() {
            return s.len();
        }
        while index < s.len() && !s.is_char_boundary(index) {
            index += 1;
        }
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_ansi_colors_and_cursor() {
        let input = "\x1B[31mRed text\x1B[0m normal \x1B[1;32mBold Green\x1B[0m\x1B[2K";
        let cleaned = TerminalScrubber::clean(input);
        assert_eq!(cleaned, "Red text normal Bold Green");
    }

    #[test]
    fn test_clean_osc_sequence() {
        let input = "\x1B]0;Terminal Title\x07Hello World";
        let cleaned = TerminalScrubber::clean(input);
        assert_eq!(cleaned, "Hello World");
    }

    #[test]
    fn test_clean_crlf_and_cr() {
        let input = "line1\r\nline2\rline3";
        let cleaned = TerminalScrubber::clean(input);
        assert_eq!(cleaned, "line1\nline2\nline3");
    }

    #[test]
    fn test_clean_fast_path() {
        let input = "clean string without escapes";
        let cleaned = TerminalScrubber::clean(input);
        assert_eq!(cleaned, input);
    }

    #[test]
    fn test_format_output_with_exit_code_and_omission() {
        let mut lines = Vec::new();
        for i in 1..=20 {
            lines.push(format!("line {}", i));
        }
        let raw = lines.join("\n");

        let formatted = TerminalScrubber::format_output(&raw, Some(0), 3, 3);
        assert!(formatted.starts_with("[Exit Code: 0]\n"));
        assert!(formatted.contains("line 1\nline 2\nline 3\n"));
        assert!(formatted.contains("[... Omitted 14 lines ...]\n"));
        assert!(formatted.contains("line 18\nline 19\nline 20"));
    }

    #[test]
    fn test_format_output_within_bounds() {
        let raw = "line a\nline b\nline c";
        let formatted = TerminalScrubber::format_output(raw, None, 5, 5);
        assert_eq!(formatted, "line a\nline b\nline c");
    }

    #[test]
    fn test_truncate_head_tail_preserves_both_ends() {
        let long_text = format!("START_{}_END", "X".repeat(10_000));
        let truncated = TerminalScrubber::truncate_head_tail(&long_text, 100);

        assert!(truncated.len() <= 100);
        assert!(truncated.starts_with("START_"));
        assert!(truncated.ends_with("_END"));
        assert!(truncated.contains("[... Omitted output exceeding byte cap ...]"));
    }
}
