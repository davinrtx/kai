//! Grep-first codebase search utility.
//!
//! Provides high-speed, regex-driven source code search honoring `.gitignore` rules
//! via the `ignore` crate, with strict 50-item and 4 KB truncation caps, search horizon bounds,
//! large-file skipping, single-pass binary detection, and cooperative steering cancellation support.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};

use kai_core::event::{GlobalSteeringReceiver, SteeringState};
use kai_core::{
    truncate_tool_output, InternalError, KaiError, OrchestratorError, Result,
    MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_OUTPUT_ITEMS,
};

use crate::window::WindowReader;

/// Default maximum file size to scan (5 MB) to avoid stalling on database dumps or datasets.
pub const DEFAULT_MAX_FILE_SIZE_BYTES: u64 = 5 * 1024 * 1024;

/// Default search horizon cap (1000 matches) to prevent full-repository scan stalls.
pub const DEFAULT_MAX_SEARCH_HORIZON: usize = 1000;

/// A single matched line in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// Relative or absolute path to the file containing the match.
    pub path: PathBuf,
    /// 1-based line number where the match occurred.
    pub line_number: usize,
    /// Stripped text content of the matching line.
    pub line_content: String,
}

impl GrepMatch {
    /// Reads surrounding code lines with `padding` lines before and after.
    ///
    /// Ideal for agent tools retrieving immediate code context around search findings.
    pub fn read_surrounding_context(&self, padding: usize) -> Result<crate::window::WindowResult> {
        WindowReader::read_context(&self.path, self.line_number, padding)
    }
}

/// Aggregated search results from a grep operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepResults {
    /// List of matched items, capped at `max_items`.
    pub matches: Vec<GrepMatch>,
    /// Total number of matches encountered before truncation or horizon cap.
    pub total_matches: usize,
    /// Number of matches omitted due to item or byte limits.
    pub omitted_count: usize,
    /// Whether results were truncated by item count or byte limits.
    pub is_truncated: bool,
    /// Whether the search was terminated early by hitting the search horizon cap.
    pub reached_horizon: bool,
}

impl GrepResults {
    /// Formats the search results into a concise string representation.
    ///
    /// Each line is formatted as `path:line:content`.
    /// If truncated, appends `[Truncated: <count> remaining items. Refine query]`.
    pub fn format(&self) -> String {
        let mut output = String::new();

        for m in &self.matches {
            output.push_str(&format!(
                "{}:{}:{}\n",
                m.path.display(),
                m.line_number,
                m.line_content
            ));
        }

        if self.is_truncated && self.omitted_count > 0 {
            if self.reached_horizon {
                output.push_str(&format!(
                    "[Truncated: {}+ remaining items (hit search horizon cap). Refine query]\n",
                    self.omitted_count
                ));
            } else {
                output.push_str(&format!(
                    "[Truncated: {} remaining items. Refine query]\n",
                    self.omitted_count
                ));
            }
        }

        if output.len() > MAX_TOOL_OUTPUT_BYTES {
            truncate_tool_output(&output)
        } else {
            output
        }
    }
}

/// Search configuration parameters.
#[derive(Debug, Clone)]
pub struct GrepOptions {
    /// Whether the regex pattern search is case-insensitive.
    pub case_insensitive: bool,
    /// Maximum number of matched lines to retain in output.
    pub max_items: usize,
    /// Maximum total matches before search terminates to prevent repository-wide stalls.
    pub max_horizon: usize,
    /// Maximum file size in bytes to inspect (defaults to 5 MB).
    pub max_file_size_bytes: u64,
    /// Maximum characters retained per individual matching line (defaults to 300).
    pub max_line_chars: usize,
    /// Optional limit on matches collected per individual file.
    pub max_matches_per_file: Option<usize>,
    /// Optional path substring filter (e.g. `"crates/core"` or `"tests"`).
    pub path_filter: Option<String>,
    /// Optional file extension filter (e.g. `["rs", "toml"]`).
    pub extensions: Option<Vec<String>>,
    /// Optional global steering receiver for cooperative cancellation.
    pub steering: Option<GlobalSteeringReceiver>,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            case_insensitive: false,
            max_items: MAX_TOOL_OUTPUT_ITEMS,
            max_horizon: DEFAULT_MAX_SEARCH_HORIZON,
            max_file_size_bytes: DEFAULT_MAX_FILE_SIZE_BYTES,
            max_line_chars: 300,
            max_matches_per_file: None,
            path_filter: None,
            extensions: None,
            steering: None,
        }
    }
}

impl GrepOptions {
    /// Attaches a global steering receiver for cooperative cancellation.
    pub fn with_steering(mut self, steering: GlobalSteeringReceiver) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Sets an optional path filter substring.
    pub fn with_path_filter(mut self, filter: impl Into<String>) -> Self {
        self.path_filter = Some(filter.into());
        self
    }

    /// Sets a maximum match limit per individual file.
    pub fn with_max_matches_per_file(mut self, limit: usize) -> Self {
        self.max_matches_per_file = Some(limit);
        self
    }

    /// Sets a maximum character limit for individual matching lines.
    pub fn with_max_line_chars(mut self, chars: usize) -> Self {
        self.max_line_chars = chars;
        self
    }
}

/// Grep search engine implementing bounded codebase traversal.
#[derive(Debug, Clone, Copy, Default)]
pub struct GrepSearcher;

impl GrepSearcher {
    /// Searches for `pattern` across files under `root` respecting `.gitignore`.
    ///
    /// # Errors
    /// Returns [`InternalError`] if the regular expression pattern is invalid.
    /// Returns [`OrchestratorError::Interrupted`] if execution is cancelled via steering signal.
    pub fn search<P: AsRef<Path>>(
        root: P,
        pattern: &str,
        options: &GrepOptions,
    ) -> Result<GrepResults> {
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(options.case_insensitive)
            .build()
            .map_err(|err| {
                KaiError::Internal(InternalError::with_cause(
                    format!("Invalid regex pattern '{}'", pattern),
                    err,
                ))
            })?;

        Self::search_with_regex(root.as_ref(), &regex, options)
    }

    /// Internal execution with a precompiled [`Regex`].
    fn search_with_regex(root: &Path, regex: &Regex, options: &GrepOptions) -> Result<GrepResults> {
        let walker = WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        let mut matches = Vec::new();
        let mut total_matches = 0usize;
        let mut reached_horizon = false;
        let max_items = options.max_items;
        let max_horizon = options.max_horizon;
        let mut line_buf = String::with_capacity(512);

        for entry_result in walker {
            // Check cancellation signal periodically
            if let Some(rx) = &options.steering {
                if *rx.borrow() == SteeringState::Terminated {
                    return Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
                        reason: "Grep search cancelled by steering signal".to_string(),
                    }));
                }
            }

            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // Path substring filtering
            if let Some(filter) = &options.path_filter {
                if !path.to_string_lossy().contains(filter) {
                    continue;
                }
            }

            // Skip files exceeding max_file_size_bytes
            if let Ok(meta) = entry.metadata() {
                if meta.len() > options.max_file_size_bytes {
                    continue;
                }
            }

            // Filter by extension if requested
            if let Some(exts) = &options.extensions {
                let file_ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if !exts.iter().any(|e| e.eq_ignore_ascii_case(file_ext)) {
                    continue;
                }
            }

            let file = match File::open(path) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let mut reader = BufReader::new(file);

            // Single-pass binary check: inspect reader's initial buffer directly
            let is_binary = match reader.fill_buf() {
                Ok(buf) => buf.contains(&0),
                Err(_) => true,
            };
            if is_binary {
                continue;
            }

            let mut line_num = 0usize;
            let mut file_matches = 0usize;

            loop {
                let read_res = WindowReader::read_bounded_line(&mut reader, &mut line_buf, 4096);
                let Ok(Some(_)) = read_res else {
                    break;
                };
                line_num += 1;

                if regex.is_match(&line_buf) {
                    total_matches += 1;

                    let allow_file_match = options
                        .max_matches_per_file
                        .map_or(true, |cap| file_matches < cap);

                    if matches.len() < max_items && allow_file_match {
                        let rel_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
                        let raw_trimmed = line_buf.trim_end();
                        let line_content = if raw_trimmed.chars().count() > options.max_line_chars {
                            let mut truncated: String =
                                raw_trimmed.chars().take(options.max_line_chars).collect();
                            truncated.push_str("...");
                            truncated
                        } else {
                            raw_trimmed.to_string()
                        };

                        matches.push(GrepMatch {
                            path: rel_path,
                            line_number: line_num,
                            line_content,
                        });
                        file_matches += 1;
                    }

                    if total_matches >= max_horizon {
                        reached_horizon = true;
                        break;
                    }
                }
            }

            if reached_horizon {
                break;
            }
        }

        let is_truncated = total_matches > max_items;
        let omitted_count = total_matches.saturating_sub(max_items);

        Ok(GrepResults {
            matches,
            total_matches,
            omitted_count,
            is_truncated,
            reached_horizon,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    #[test]
    fn test_grep_search_and_ignore() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_suite_3");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let src_dir = temp_dir.join("src");
        create_dir_all(&src_dir).expect("create src dir");

        write(
            src_dir.join("main.rs"),
            "fn main() {\n    let target = 42;\n    println!(\"{}\", target);\n}\n",
        )
        .expect("write main.rs");

        write(
            src_dir.join("lib.rs"),
            "pub const TARGET: &str = \"kai\";\n",
        )
        .expect("write lib.rs");

        let options = GrepOptions {
            case_insensitive: true,
            max_items: 50,
            extensions: None,
            ..Default::default()
        };

        let results = GrepSearcher::search(&temp_dir, "target", &options).expect("grep search");
        assert_eq!(results.total_matches, 3);
        assert_eq!(results.matches.len(), 3);
        assert!(!results.is_truncated);

        let formatted = results.format();
        assert!(formatted.contains("target"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_truncation_cap() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_truncation_3");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let mut lines = Vec::new();
        for i in 1..=60 {
            lines.push(format!("match line {}", i));
        }
        write(temp_dir.join("data.txt"), lines.join("\n")).expect("write data.txt");

        let options = GrepOptions {
            case_insensitive: false,
            max_items: 10,
            ..Default::default()
        };

        let results = GrepSearcher::search(&temp_dir, "match line", &options).expect("search");
        assert_eq!(results.total_matches, 60);
        assert_eq!(results.matches.len(), 10);
        assert!(results.is_truncated);
        assert_eq!(results.omitted_count, 50);

        let formatted = results.format();
        assert!(formatted.contains("[Truncated: 50 remaining items. Refine query]"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_search_horizon() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_horizon_3");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let mut lines = Vec::new();
        for i in 1..=100 {
            lines.push(format!("horizon target {}", i));
        }
        write(temp_dir.join("big.txt"), lines.join("\n")).expect("write big.txt");

        let options = GrepOptions {
            max_items: 5,
            max_horizon: 20,
            ..Default::default()
        };

        let results = GrepSearcher::search(&temp_dir, "horizon target", &options).expect("search");
        assert_eq!(results.matches.len(), 5);
        assert!(results.total_matches >= 20);
        assert!(results.reached_horizon);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_steering_cancellation() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_cancel_3");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        write(temp_dir.join("file.txt"), "hello world\n").expect("write file");

        let (tx, rx) = kai_core::event::global_steering_channel();
        tx.send(SteeringState::Terminated).expect("send cancel");

        let options = GrepOptions::default().with_steering(rx);
        let res = GrepSearcher::search(&temp_dir, "hello", &options);
        assert!(matches!(
            res,
            Err(KaiError::Orchestrator(
                OrchestratorError::Interrupted { .. }
            ))
        ));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_per_file_match_cap() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_per_file");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let mut lines = Vec::new();
        for i in 1..=20 {
            lines.push(format!("recurrent line {}", i));
        }
        write(temp_dir.join("f1.txt"), lines.join("\n")).expect("write f1");
        write(temp_dir.join("f2.txt"), "recurrent line in f2\n").expect("write f2");

        let options = GrepOptions::default().with_max_matches_per_file(2);
        let results = GrepSearcher::search(&temp_dir, "recurrent", &options).expect("search");

        let f1_count = results
            .matches
            .iter()
            .filter(|m| m.path.to_string_lossy().contains("f1.txt"))
            .count();
        assert_eq!(f1_count, 2);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_match_surrounding_context() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_ctx");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let file_path = temp_dir.join("code.rs");
        let code = "fn alpha() {}\nfn beta() {\n    let needle = 1;\n}\nfn gamma() {}\n";
        write(&file_path, code).expect("write code");

        let options = GrepOptions::default();
        let results = GrepSearcher::search(&temp_dir, "needle", &options).expect("search");
        assert_eq!(results.matches.len(), 1);

        // Note: GrepMatch path is relative to root, so we check with root
        let match_item = &results.matches[0];
        let full_path = temp_dir.join(&match_item.path);
        let ctx = WindowReader::read_context(&full_path, match_item.line_number, 1)
            .expect("read context");
        assert_eq!(ctx.start_line, 2);
        assert_eq!(ctx.end_line, 4);
        assert!(ctx.content.contains("let needle = 1;"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_grep_match_line_bounding() {
        let temp_dir = std::env::temp_dir().join("kai_test_grep_bound_line");
        let _ = std::fs::remove_dir_all(&temp_dir);
        create_dir_all(&temp_dir).expect("create temp dir");

        let long_line = format!("prefix_target_{}", "Z".repeat(1000));
        write(temp_dir.join("long.txt"), format!("{}\n", long_line)).expect("write long");

        let options = GrepOptions::default().with_max_line_chars(50);
        let results = GrepSearcher::search(&temp_dir, "prefix_target", &options).expect("search");
        assert_eq!(results.matches.len(), 1);
        let content = &results.matches[0].line_content;
        assert!(content.len() <= 55); // 50 chars + "..."
        assert!(content.ends_with("..."));

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
