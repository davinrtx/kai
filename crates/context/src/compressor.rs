//! Semantic command output compression and noise reduction.
//!
//! Provides deterministic pruning of compiler boilerplate, passing test noise,
//! and verbose VCS metadata to minimize LLM token consumption while strictly
//! preserving critical diagnostic context (errors, failures, warnings, and traces).

use kai_core::traits::CommandOutputCompressor;

use crate::scrubber::TerminalScrubber;

/// High-performance semantic compressor for command execution output streams.
#[derive(Debug, Clone, Default)]
pub struct SemanticCommandCompressor;

impl SemanticCommandCompressor {
    /// Constructs a new [`SemanticCommandCompressor`].
    pub fn new() -> Self {
        Self
    }

    /// Extracts the primary binary command name from a shell command string.
    fn extract_primary_cmd(command: &str) -> (&str, &str) {
        let trimmed = command.trim();
        let mut parts = trimmed.split_whitespace();
        let first = parts.next().unwrap_or("");
        let rest = trimmed.strip_prefix(first).unwrap_or("").trim_start();

        // Extract binary basename (handling both '/' and '\' cross-platform without allocations)
        let name_start = first.rfind(['/', '\\']).map(|idx| idx + 1).unwrap_or(0);
        let raw_name = &first[name_start..];
        let base = raw_name.strip_suffix(".exe").unwrap_or(raw_name);

        (base, rest)
    }

    /// Compresses Rust `cargo` command outputs (`cargo test`, `cargo check`, `cargo build`, `cargo clippy`).
    fn compress_cargo(subcommand_args: &str, output: &str, exit_code: Option<i32>) -> String {
        let sub = subcommand_args.split_whitespace().next().unwrap_or("");
        match sub {
            "test" => Self::compress_cargo_test(output, exit_code),
            "check" | "build" | "clippy" | "run" => Self::compress_cargo_build(output, exit_code),
            _ => Self::compress_generic(output, exit_code),
        }
    }

    /// Filters `cargo check`, `cargo build`, and `cargo clippy` output streams.
    ///
    /// Removes repetitive `Compiling`, `Checking`, `Downloading` lines while preserving
    /// all diagnostic blocks (`warning:`, `error:`, location pointers, code snippets)
    /// and final compiler status.
    fn compress_cargo_build(output: &str, exit_code: Option<i32>) -> String {
        let mut preserved = Vec::new();
        let mut has_diagnostics = false;

        let is_noise = |line: &str| -> bool {
            let t = line.trim_start();
            t.starts_with("Compiling ")
                || t.starts_with("Checking ")
                || t.starts_with("Downloading ")
                || t.starts_with("Downloaded ")
                || t.starts_with("Updating ")
                || t.starts_with("Locking ")
                || t.starts_with("Blocking waiting for file lock")
                || (t.starts_with("Finished ") && exit_code.unwrap_or(0) == 0)
        };

        for line in output.lines() {
            let t = line.trim();
            if t.starts_with("warning:")
                || t.starts_with("error:")
                || t.starts_with("error[E")
                || t.contains("--> ")
            {
                has_diagnostics = true;
            }

            if !is_noise(line) {
                preserved.push(line);
            }
        }

        if !has_diagnostics && exit_code.unwrap_or(0) == 0 {
            return "[Build/Check succeeded with 0 errors and 0 warnings]".to_string();
        }

        let joined = preserved.join("\n");
        Self::apply_bounds_if_needed(&joined, exit_code)
    }

    /// Filters `cargo test` outputs.
    ///
    /// Suppresses hundreds of passing test cases (`test ... ok`), preserving only
    /// failures (`test ... FAILED`), panic messages, assertion traces, and the summary table.
    fn compress_cargo_test(output: &str, exit_code: Option<i32>) -> String {
        let mut filtered_lines = Vec::new();
        let mut in_failure_section = false;
        let mut failure_count = 0;
        let mut passed_count = 0;

        for line in output.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("test ") && trimmed.ends_with(" ... ok") {
                passed_count += 1;
                continue;
            }

            if trimmed.starts_with("test ") && trimmed.ends_with(" ... FAILED") {
                failure_count += 1;
                filtered_lines.push(line);
                continue;
            }

            if trimmed.starts_with("failures:") {
                in_failure_section = true;
                filtered_lines.push(line);
                continue;
            }

            if in_failure_section && trimmed.starts_with("test result:") {
                in_failure_section = false;
            }

            // Always keep compiler warnings/errors during test compilation
            if trimmed.starts_with("error:") || trimmed.starts_with("warning:") {
                filtered_lines.push(line);
                continue;
            }

            // Keep doc test headers and result summaries
            if trimmed.starts_with("test result:")
                || trimmed.starts_with("running ")
                || in_failure_section
            {
                filtered_lines.push(line);
            }
        }

        if failure_count == 0 && exit_code.unwrap_or(0) == 0 {
            let summary = filtered_lines
                .iter()
                .find(|l| l.contains("test result:"))
                .copied()
                .unwrap_or("test result: ok.");
            return format!("[All {passed_count} test(s) passed successfully]\n{summary}");
        }

        let joined = filtered_lines.join("\n");
        Self::apply_bounds_if_needed(&joined, exit_code)
    }

    /// Compresses Git outputs (`git status`, `git diff`, `git log`).
    fn compress_git(subcommand_args: &str, output: &str, exit_code: Option<i32>) -> String {
        let sub = subcommand_args.split_whitespace().next().unwrap_or("");
        match sub {
            "status" => Self::compress_git_status(output),
            "diff" => Self::compress_git_diff(output, exit_code),
            "log" => Self::compress_git_log(output, exit_code),
            _ => Self::compress_generic(output, exit_code),
        }
    }

    /// Filters interactive advice hints from `git status` output.
    fn compress_git_status(output: &str) -> String {
        let mut preserved = Vec::new();

        let is_advice = |line: &str| -> bool {
            let t = line.trim();
            t.starts_with("(use \"git ")
                || t.starts_with("(commit or discard")
                || t.starts_with("(all conflicts fixed")
        };

        for line in output.lines() {
            if !is_advice(line) {
                preserved.push(line);
            }
        }

        preserved.join("\n")
    }

    /// Compresses verbose index hashes and mode metadata from `git diff` output.
    fn compress_git_diff(output: &str, exit_code: Option<i32>) -> String {
        let mut preserved = Vec::new();

        for line in output.lines() {
            let t = line.trim();
            if t.starts_with("index ")
                || t.starts_with("old mode ")
                || t.starts_with("new mode ")
                || t.starts_with("similarity index ")
            {
                continue;
            }
            preserved.push(line);
        }

        let joined = preserved.join("\n");
        Self::apply_bounds_if_needed(&joined, exit_code)
    }

    /// Compresses multi-line `git log` entries into compact single-line summaries.
    fn compress_git_log(output: &str, exit_code: Option<i32>) -> String {
        let lines: Vec<&str> = output.lines().collect();
        if lines.is_empty() {
            return String::new();
        }

        // If log is already formatted as oneline, preserve directly
        if lines
            .iter()
            .all(|l| !l.starts_with("commit ") || l.len() < 50)
        {
            return Self::apply_bounds_if_needed(output, exit_code);
        }

        let mut oneline_records = Vec::new();
        let mut current_hash = String::new();
        let mut current_msg = String::new();

        for line in lines {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("commit ") {
                if !current_hash.is_empty() {
                    let short_hash = &current_hash[..current_hash.len().min(7)];
                    oneline_records.push(format!("{short_hash} {current_msg}"));
                    current_msg.clear();
                }
                current_hash = rest.split_whitespace().next().unwrap_or("").to_string();
            } else if !t.starts_with("Author:")
                && !t.starts_with("Date:")
                && !t.is_empty()
                && current_msg.is_empty()
            {
                current_msg = t.to_string();
            }
        }

        if !current_hash.is_empty() {
            let short_hash = &current_hash[..current_hash.len().min(7)];
            oneline_records.push(format!("{short_hash} {current_msg}"));
        }

        let joined = oneline_records.join("\n");
        Self::apply_bounds_if_needed(&joined, exit_code)
    }

    /// Compresses generic test runners (`pytest`, `jest`, `vitest`, `go test`).
    fn compress_test_runner(output: &str, exit_code: Option<i32>) -> String {
        let mut preserved = Vec::new();
        for line in output.lines() {
            let t = line.trim();
            // Suppress Go test passing indicators
            if t.starts_with("=== RUN") || t.starts_with("--- PASS:") {
                continue;
            }
            // Suppress Jest/Vitest passing lines
            if t.starts_with("✓ ") || t.starts_with("PASS ") {
                continue;
            }
            preserved.push(line);
        }

        let joined = preserved.join("\n");
        Self::apply_bounds_if_needed(&joined, exit_code)
    }

    /// Applies generic terminal scrubbing and bounded head/tail preservation.
    fn compress_generic(output: &str, exit_code: Option<i32>) -> String {
        let cleaned = TerminalScrubber::clean(output);
        Self::apply_bounds_if_needed(&cleaned, exit_code)
    }

    /// Ensures that output adheres to the 50-line / 4 KB operational bounds,
    /// preserving head and tail diagnostic context if truncation occurs.
    fn apply_bounds_if_needed(text: &str, exit_code: Option<i32>) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        let max_lines = 50;
        if total > max_lines {
            let head_count = 20;
            let tail_count = 25;
            let omitted = total.saturating_sub(head_count + tail_count);

            let mut out = String::new();
            for l in &lines[..head_count] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(&format!(
                "[... Omitted {omitted} lines of repetitive output ...]\n"
            ));
            for l in &lines[total - tail_count..] {
                out.push_str(l);
                out.push('\n');
            }

            if let Some(code) = exit_code {
                if code != 0 && !out.contains("Exit Code:") {
                    out.push_str(&format!("[Exit Code: {code}]"));
                }
            }
            out.trim_end().to_string()
        } else {
            let mut res = text.trim_end().to_string();
            if let Some(code) = exit_code {
                if code != 0 && !res.contains("Exit Code:") {
                    if !res.is_empty() {
                        res.push('\n');
                    }
                    res.push_str(&format!("[Exit Code: {code}]"));
                }
            }
            res
        }
    }
}

impl CommandOutputCompressor for SemanticCommandCompressor {
    fn compress(&self, command: &str, output: &str, exit_code: Option<i32>) -> String {
        let cleaned = TerminalScrubber::clean(output);
        let (primary, args) = Self::extract_primary_cmd(command);

        match primary {
            "cargo" => Self::compress_cargo(args, &cleaned, exit_code),
            "git" => Self::compress_git(args, &cleaned, exit_code),
            "pytest" | "go" | "jest" | "vitest" | "npm" | "npx" | "pnpm" | "bun" => {
                Self::compress_test_runner(&cleaned, exit_code)
            }
            _ => Self::compress_generic(&cleaned, exit_code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_primary_cmd() {
        assert_eq!(
            SemanticCommandCompressor::extract_primary_cmd("cargo test --lib"),
            ("cargo", "test --lib")
        );
        assert_eq!(
            SemanticCommandCompressor::extract_primary_cmd("C:\\tools\\git.exe status"),
            ("git", "status")
        );
        assert_eq!(
            SemanticCommandCompressor::extract_primary_cmd("/usr/bin/cargo check"),
            ("cargo", "check")
        );
    }

    #[test]
    fn test_compress_cargo_check_success() {
        let compressor = SemanticCommandCompressor::new();
        let raw = "\
   Compiling libc v0.2.155
   Compiling regex v1.10.4
   Compiling kai-core v0.6.4
    Checking kai-context v0.6.4
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.15s";

        let compressed = compressor.compress("cargo check", raw, Some(0));
        assert!(compressed.contains("Build/Check succeeded"));
        assert!(!compressed.contains("Compiling libc"));
    }

    #[test]
    fn test_compress_cargo_check_with_errors() {
        let compressor = SemanticCommandCompressor::new();
        let raw = "\
   Compiling libc v0.2.155
   Compiling kai-core v0.6.4
error[E0425]: cannot find value `foo` in this scope
  --> src/main.rs:10:5
   |
10 |     foo
   |     ^^^ not found in this scope
error: could not compile `kai` (bin \"kai\") due to 1 previous error";

        let compressed = compressor.compress("cargo check", raw, Some(101));
        assert!(compressed.contains("cannot find value `foo`"));
        assert!(compressed.contains("--> src/main.rs:10:5"));
        assert!(!compressed.contains("Compiling libc"));
        assert!(compressed.contains("[Exit Code: 101]"));
    }

    #[test]
    fn test_compress_cargo_test_success() {
        let compressor = SemanticCommandCompressor::new();
        let mut raw = String::new();
        for i in 0..50 {
            raw.push_str(&format!("test module::test_{i} ... ok\n"));
        }
        raw.push_str("\ntest result: ok. 50 passed; 0 failed; finished in 0.05s\n");

        let compressed = compressor.compress("cargo test", &raw, Some(0));
        assert!(compressed.contains("All 50 test(s) passed successfully"));
        assert!(!compressed.contains("test module::test_0 ... ok"));
    }

    #[test]
    fn test_compress_cargo_test_failure() {
        let compressor = SemanticCommandCompressor::new();
        let raw = "\
running 3 tests
test test_alpha ... ok
test test_beta ... FAILED
test test_gamma ... ok

failures:

---- test_beta stdout ----
thread 'test_beta' panicked at src/lib.rs:25:9:
assertion `left == right` failed
  left: 42
 right: 0

failures:
    test_beta

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s";

        let compressed = compressor.compress("cargo test", raw, Some(101));
        assert!(compressed.contains("test test_beta ... FAILED"));
        assert!(compressed.contains("assertion `left == right` failed"));
        assert!(compressed.contains("test result: FAILED"));
        assert!(!compressed.contains("test test_alpha ... ok"));
        assert!(!compressed.contains("test test_gamma ... ok"));
    }

    #[test]
    fn test_compress_git_status_advice_removal() {
        let compressor = SemanticCommandCompressor::new();
        let raw = "\
On branch main
Changes to be committed:
  (use \"git restore --staged <file>...\" to unstage)
\tmodified:   src/lib.rs

Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
\tnewfile.txt";

        let compressed = compressor.compress("git status", raw, Some(0));
        assert!(compressed.contains("On branch main"));
        assert!(compressed.contains("modified:   src/lib.rs"));
        assert!(compressed.contains("newfile.txt"));
        assert!(!compressed.contains("(use \"git restore"));
        assert!(!compressed.contains("(use \"git add"));
    }
}
