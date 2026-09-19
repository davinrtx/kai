//! Robust source code patcher using fuzzy search/replace blocks.
//!
//! Provides multi-tier matching (exact, whitespace-normalized, and sliding-window Levenshtein)
//! to substitute code blocks without relying on brittle unified diff line numbers.
//! Ensures transactional filesystem safety via atomic temporary file renaming.

use std::path::Path;

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::traits::{CodePatcher, PatchApplicationResult, PatchBlock};

/// Default similarity threshold for fuzzy window matching (85%).
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.85;

/// Resilient source code patcher executing search-and-replace block substitutions.
#[derive(Debug, Clone)]
pub struct FuzzyBlockPatcher {
    similarity_threshold: f64,
}

impl FuzzyBlockPatcher {
    /// Constructs a new [`FuzzyBlockPatcher`] with the specified similarity threshold.
    pub fn new(similarity_threshold: f64) -> Self {
        Self {
            similarity_threshold: similarity_threshold.clamp(0.5, 1.0),
        }
    }

    /// Parses patch text containing standard `<<<<<<< SEARCH ... ======= ... >>>>>>> REPLACE` markers.
    pub fn parse_blocks(patch_text: &str) -> Vec<PatchBlock> {
        let mut blocks = Vec::new();
        let lines: Vec<&str> = patch_text.lines().collect();

        let mut idx = 0;
        while idx < lines.len() {
            let line = lines[idx].trim();
            if line.starts_with("<<<<<<< SEARCH") || line == "<<<<<<<" {
                idx += 1;
                let mut search_lines = Vec::new();
                while idx < lines.len() {
                    let s_line = lines[idx];
                    if s_line.trim() == "=======" {
                        idx += 1;
                        break;
                    }
                    search_lines.push(s_line);
                    idx += 1;
                }

                let mut replace_lines = Vec::new();
                while idx < lines.len() {
                    let r_line = lines[idx];
                    if r_line.trim().starts_with(">>>>>>>") {
                        idx += 1;
                        break;
                    }
                    replace_lines.push(r_line);
                    idx += 1;
                }

                blocks.push(PatchBlock {
                    search: search_lines.join("\n"),
                    replace: replace_lines.join("\n"),
                });
            } else {
                idx += 1;
            }
        }

        blocks
    }

    /// Calculates normalized Levenshtein similarity between two strings [0.0, 1.0].
    pub fn normalized_similarity(a: &str, b: &str) -> f64 {
        if a == b {
            return 1.0;
        }
        let len_a = a.chars().count();
        let len_b = b.chars().count();
        let max_len = len_a.max(len_b);
        if max_len == 0 {
            return 1.0;
        }

        let dist = Self::levenshtein_distance(a, b);
        1.0 - (dist as f64 / max_len as f64)
    }

    /// Computes Levenshtein edit distance with O(min(m, n)) space complexity.
    pub fn levenshtein_distance(a: &str, b: &str) -> usize {
        let a_chars: Vec<char> = a.chars().collect();
        let b_chars: Vec<char> = b.chars().collect();

        let m = a_chars.len();
        let n = b_chars.len();

        if m == 0 {
            return n;
        }
        if n == 0 {
            return m;
        }

        let mut prev_row: Vec<usize> = (0..=n).collect();
        let mut curr_row: Vec<usize> = vec![0; n + 1];

        for i in 1..=m {
            curr_row[0] = i;
            for j in 1..=n {
                let cost = if a_chars[i - 1] == b_chars[j - 1] {
                    0
                } else {
                    1
                };
                curr_row[j] = (prev_row[j] + 1)
                    .min(curr_row[j - 1] + 1)
                    .min(prev_row[j - 1] + cost);
            }
            prev_row.copy_from_slice(&curr_row);
        }

        prev_row[n]
    }

    /// Normalizes lines by stripping leading/trailing whitespace and normalizing CRLF.
    fn normalize_lines(text: &str) -> Vec<String> {
        text.lines().map(|l| l.trim().to_string()).collect()
    }

    /// Transactionally applies patch blocks to a file on disk via atomic replace.
    pub fn apply_to_file(
        &self,
        target_path: &Path,
        patch_blocks: &[PatchBlock],
    ) -> Result<PatchApplicationResult> {
        let original_content = std::fs::read_to_string(target_path).map_err(KaiError::Io)?;
        let result = self.apply_blocks(target_path, &original_content, patch_blocks)?;

        let parent = target_path.parent().unwrap_or_else(|| Path::new("."));
        let pid = std::process::id();
        let ts = kai_core::message::current_timestamp_ms();
        let file_name = target_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file");
        let tmp_path = parent.join(format!(".tmp.{pid}.{ts}.{file_name}"));

        std::fs::write(&tmp_path, &result.modified_content).map_err(KaiError::Io)?;
        std::fs::rename(&tmp_path, target_path).map_err(|err| {
            let _ = std::fs::remove_file(&tmp_path);
            KaiError::Io(err)
        })?;

        Ok(result)
    }
}

impl Default for FuzzyBlockPatcher {
    fn default() -> Self {
        Self::new(DEFAULT_SIMILARITY_THRESHOLD)
    }
}

impl CodePatcher for FuzzyBlockPatcher {
    fn apply_blocks<'a>(
        &'a self,
        file_path: &'a Path,
        content: &'a str,
        patch_blocks: &'a [PatchBlock],
    ) -> Result<PatchApplicationResult> {
        if patch_blocks.is_empty() {
            return Ok(PatchApplicationResult {
                modified_content: content.to_string(),
                applied_count: 0,
                confidence_score: 1.0,
            });
        }

        let mut current_text = content.to_string();
        let mut applied_count = 0;
        let mut total_confidence = 0.0;

        for (block_idx, block) in patch_blocks.iter().enumerate() {
            let search_trimmed = block.search.trim();
            if search_trimmed.is_empty() {
                return Err(KaiError::Tool(ToolError::ExecutionFailed {
                    name: "apply_patch".to_string(),
                    reason: format!(
                        "Patch block #{} in {} has an empty SEARCH block",
                        block_idx + 1,
                        file_path.display()
                    ),
                }));
            }

            // Tier 1: Exact substring match
            if let Some(pos) = current_text.find(&block.search) {
                let mut updated = String::with_capacity(current_text.len() + block.replace.len());
                updated.push_str(&current_text[..pos]);
                updated.push_str(&block.replace);
                updated.push_str(&current_text[pos + block.search.len()..]);
                current_text = updated;
                applied_count += 1;
                total_confidence += 1.0;
                continue;
            }

            // Tier 2: Whitespace-normalized line matching
            let file_lines = Self::normalize_lines(&current_text);
            let search_lines = Self::normalize_lines(&block.search);
            let search_len = search_lines.len();

            if search_len == 0 || search_len > file_lines.len() {
                return Err(KaiError::Tool(ToolError::ExecutionFailed {
                    name: "apply_patch".to_string(),
                    reason: format!(
                        "Block #{} could not match: search block is longer than file {}",
                        block_idx + 1,
                        file_path.display()
                    ),
                }));
            }

            let mut best_match: Option<(usize, f64)> = None;

            for start_idx in 0..=(file_lines.len() - search_len) {
                let window = &file_lines[start_idx..start_idx + search_len];
                let window_str = window.join("\n");
                let target_str = search_lines.join("\n");

                let score = Self::normalized_similarity(&window_str, &target_str);
                if score >= self.similarity_threshold {
                    match best_match {
                        Some((_, prev_score)) if score > prev_score => {
                            best_match = Some((start_idx, score));
                        }
                        None => {
                            best_match = Some((start_idx, score));
                        }
                        _ => {}
                    }
                }
            }

            if let Some((match_start_line, score)) = best_match {
                let raw_lines: Vec<&str> = current_text.lines().collect();
                let mut reconstructed = String::new();

                for (idx, line) in raw_lines.iter().enumerate() {
                    if idx == match_start_line {
                        reconstructed.push_str(&block.replace);
                        if !block.replace.ends_with('\n') && idx + search_len < raw_lines.len() {
                            reconstructed.push('\n');
                        }
                    } else if idx > match_start_line && idx < match_start_line + search_len {
                        continue;
                    } else {
                        reconstructed.push_str(line);
                        reconstructed.push('\n');
                    }
                }

                current_text = reconstructed;
                applied_count += 1;
                total_confidence += score;
            } else {
                return Err(KaiError::Tool(ToolError::ExecutionFailed {
                    name: "apply_patch".to_string(),
                    reason: format!(
                        "Fuzzy patch failed on {}: block #{} did not meet similarity threshold ({:.2})",
                        file_path.display(),
                        block_idx + 1,
                        self.similarity_threshold
                    ),
                }));
            }
        }

        let avg_confidence = if applied_count > 0 {
            total_confidence / applied_count as f64
        } else {
            0.0
        };

        Ok(PatchApplicationResult {
            modified_content: current_text,
            applied_count,
            confidence_score: avg_confidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_search_replace() {
        let patcher = FuzzyBlockPatcher::default();
        let content = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let blocks = vec![PatchBlock::new(
            "    a + b",
            "    // calculate sum\n    a + b",
        )];

        let res = patcher
            .apply_blocks(Path::new("math.rs"), content, &blocks)
            .unwrap();
        assert_eq!(res.applied_count, 1);
        assert_eq!(res.confidence_score, 1.0);
        assert!(res.modified_content.contains("// calculate sum"));
    }

    #[test]
    fn test_whitespace_drift_fuzzy_match() {
        let patcher = FuzzyBlockPatcher::default();
        let content = "fn calculate() {\n    let x = 10;   \n    let y = 20;\n}\n";
        // Search block has different trailing spaces and indentation
        let blocks = vec![PatchBlock::new(
            "let x = 10;\nlet y = 20;",
            "let x = 15;\nlet y = 25;",
        )];

        let res = patcher
            .apply_blocks(Path::new("calc.rs"), content, &blocks)
            .unwrap();
        assert_eq!(res.applied_count, 1);
        assert!(res.confidence_score >= 0.85);
        assert!(res.modified_content.contains("let x = 15;"));
    }

    #[test]
    fn test_parse_blocks_markers() {
        let raw_patch = r#"
<<<<<<< SEARCH
def old_function():
    return 42
=======
def new_function():
    return 100
>>>>>>> REPLACE
"#;
        let blocks = FuzzyBlockPatcher::parse_blocks(raw_patch);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].search, "def old_function():\n    return 42");
        assert_eq!(blocks[0].replace, "def new_function():\n    return 100");
    }
}
