//! Transactional unified diff patch application tool.
//!
//! Applies unified diff patches using atomic sibling temporary files and
//! atomic `std::fs::rename`, guaranteeing zero in-place corruption and automatic rollback.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Arguments accepted by [`ApplyPatchTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPatchArgs {
    /// Path to the target file to modify.
    pub path: String,
    /// Unified diff patch content.
    pub patch: String,
}

/// A parsed unified diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// 1-based starting line number in original file.
    pub old_start: usize,
    /// Number of lines affected in original file.
    pub old_count: usize,
    /// 1-based starting line number in new file.
    pub new_start: usize,
    /// Number of lines affected in new file.
    pub new_count: usize,
    /// Lines comprising this hunk (each with prefix ' ', '-', '+', or '\').
    pub lines: Vec<String>,
}

/// Tool for applying unified diff patches atomically.
#[derive(Debug, Clone, Default)]
pub struct ApplyPatchTool;

impl ApplyPatchTool {
    /// Constructs a new [`ApplyPatchTool`].
    pub fn new() -> Self {
        Self
    }

    /// Resolves the file path relative to the active working directory if relative.
    fn resolve_path(base_dir: &Path, requested: &str) -> PathBuf {
        let p = Path::new(requested);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            base_dir.join(p)
        }
    }

    /// Parses a unified diff string into individual hunks.
    pub fn parse_unified_diff(diff_text: &str) -> std::result::Result<Vec<DiffHunk>, String> {
        let mut hunks = Vec::new();
        let mut current_hunk: Option<DiffHunk> = None;

        for line in diff_text.lines() {
            if line.starts_with("@@") && line[2..].contains("@@") {
                if let Some(hunk) = current_hunk.take() {
                    hunks.push(hunk);
                }

                let parsed_hunk = Self::parse_hunk_header(line)?;
                current_hunk = Some(parsed_hunk);
            } else if let Some(ref mut hunk) = current_hunk {
                if line.starts_with(' ')
                    || line.starts_with('+')
                    || line.starts_with('-')
                    || line.starts_with('\\')
                {
                    hunk.lines.push(line.to_string());
                } else if line.is_empty() {
                    // Blank line inside hunk without leading space (common formatting variation)
                    hunk.lines.push(" ".to_string());
                }
            }
        }

        if let Some(hunk) = current_hunk.take() {
            hunks.push(hunk);
        }

        if hunks.is_empty() {
            return Err("No valid diff hunks found in patch".to_string());
        }

        Ok(hunks)
    }

    /// Parses a single hunk header (e.g. `@@ -1,5 +1,6 @@`).
    fn parse_hunk_header(header: &str) -> std::result::Result<DiffHunk, String> {
        let trimmed = header.trim();
        let parts: Vec<&str> = trimmed.split("@@").collect();
        if parts.len() < 3 {
            return Err(format!("Malformed hunk header: '{header}'"));
        }

        let range_part = parts[1].trim();
        let mut old_start = 1usize;
        let mut old_count = 1usize;
        let mut new_start = 1usize;
        let mut new_count = 1usize;

        for token in range_part.split_whitespace() {
            if let Some(stripped) = token.strip_prefix('-') {
                let (s, c) = Self::parse_range_pair(stripped)?;
                old_start = s;
                old_count = c;
            } else if let Some(stripped) = token.strip_prefix('+') {
                let (s, c) = Self::parse_range_pair(stripped)?;
                new_start = s;
                new_count = c;
            }
        }

        Ok(DiffHunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines: Vec::new(),
        })
    }

    /// Parses a `start,count` or single `start` pair.
    fn parse_range_pair(pair: &str) -> std::result::Result<(usize, usize), String> {
        let parts: Vec<&str> = pair.split(',').collect();
        match parts.as_slice() {
            [s] => {
                let start: usize = s
                    .parse()
                    .map_err(|_| format!("Invalid line number: '{s}'"))?;
                Ok((start, 1))
            }
            [s, c] => {
                let start: usize = s
                    .parse()
                    .map_err(|_| format!("Invalid line number: '{s}'"))?;
                let count: usize = c
                    .parse()
                    .map_err(|_| format!("Invalid line count: '{c}'"))?;
                Ok((start, count))
            }
            _ => Err(format!("Invalid range token: '{pair}'")),
        }
    }

    /// Applies parsed hunks to the original content string, returning the modified content.
    pub fn apply_hunks(original: &str, hunks: &[DiffHunk]) -> std::result::Result<String, String> {
        let orig_lines: Vec<&str> = original.lines().collect();
        let ends_with_newline = original.ends_with('\n') || original.is_empty();

        let mut output_lines = Vec::new();
        let mut orig_idx = 0usize;

        for (hunk_idx, hunk) in hunks.iter().enumerate() {
            let hunk_old_start_0based = if hunk.old_start == 0 {
                0
            } else {
                hunk.old_start.saturating_sub(1)
            };

            if hunk_old_start_0based < orig_idx {
                return Err(format!(
                    "Hunk {} targets line {} which is before the current line position {} (overlapping or out-of-order hunks)",
                    hunk_idx + 1,
                    hunk.old_start,
                    orig_idx + 1
                ));
            }

            // Copy unaffected lines before this hunk
            if orig_idx < hunk_old_start_0based {
                if hunk_old_start_0based > orig_lines.len() {
                    return Err(format!(
                        "Hunk {} targets line {} which is beyond end of file ({} lines)",
                        hunk_idx + 1,
                        hunk.old_start,
                        orig_lines.len()
                    ));
                }
                output_lines.extend_from_slice(&orig_lines[orig_idx..hunk_old_start_0based]);
                orig_idx = hunk_old_start_0based;
            }

            for line in &hunk.lines {
                if let Some(context) = line.strip_prefix(' ') {
                    if orig_idx >= orig_lines.len() {
                        return Err(format!(
                            "Hunk {} context match failed: expected '{}' but reached end of file",
                            hunk_idx + 1,
                            context
                        ));
                    }
                    if orig_lines[orig_idx] != context {
                        return Err(format!(
                            "Hunk {} context mismatch at line {}: expected '{}', found '{}'",
                            hunk_idx + 1,
                            orig_idx + 1,
                            context,
                            orig_lines[orig_idx]
                        ));
                    }
                    output_lines.push(orig_lines[orig_idx]);
                    orig_idx += 1;
                } else if let Some(removed) = line.strip_prefix('-') {
                    if orig_idx >= orig_lines.len() {
                        return Err(format!(
                            "Hunk {} deletion match failed: expected '{}' but reached end of file",
                            hunk_idx + 1,
                            removed
                        ));
                    }
                    if orig_lines[orig_idx] != removed {
                        return Err(format!(
                            "Hunk {} deletion mismatch at line {}: expected '{}', found '{}'",
                            hunk_idx + 1,
                            orig_idx + 1,
                            removed,
                            orig_lines[orig_idx]
                        ));
                    }
                    orig_idx += 1;
                } else if let Some(added) = line.strip_prefix('+') {
                    output_lines.push(added);
                }
                // Ignore '\ No newline at end of file'
            }
        }

        // Copy any remaining lines
        if orig_idx < orig_lines.len() {
            output_lines.extend_from_slice(&orig_lines[orig_idx..]);
        }

        let line_sep = if original.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut result = output_lines.join(line_sep);
        if ends_with_newline && !result.is_empty() {
            result.push_str(line_sep);
        }

        Ok(result)
    }
}

impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        "Applies a SEARCH/REPLACE block patch or unified diff patch to a file with transactional atomic replacement and automatic failure rollback."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path", "patch"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path of the target file to modify"
                },
                "patch": {
                    "type": "string",
                    "description": "SEARCH/REPLACE blocks (<<<<<<< SEARCH ... ======= ... >>>>>>> REPLACE) or unified diff patch content"
                }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileWrite
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object".to_string(),
            });
        }

        let args: ApplyPatchArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        if args.path.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "path cannot be empty".to_string(),
            });
        }

        if args.patch.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "patch cannot be empty".to_string(),
            });
        }

        Ok(())
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            context.check_cancellation()?;

            let args: ApplyPatchArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            if args.path.trim().is_empty() {
                return Ok(ToolResult::error(self.name(), "path cannot be empty"));
            }

            if args.patch.trim().is_empty() {
                return Ok(ToolResult::error(self.name(), "patch cannot be empty"));
            }

            let target_path = Self::resolve_path(context.working_dir(), &args.path);

            let original_content = if target_path.exists() {
                match tokio::fs::read_to_string(&target_path).await {
                    Ok(c) => c,
                    Err(err) => {
                        return Ok(ToolResult::error(
                            self.name(),
                            format!("Failed to read target file: {err}"),
                        ));
                    }
                }
            } else {
                String::new()
            };

            let (patched_content, summary) = if args.patch.contains("<<<<<<<") {
                let blocks = crate::patcher::FuzzyBlockPatcher::parse_blocks(&args.patch);
                if blocks.is_empty() {
                    return Ok(ToolResult::error(
                        self.name(),
                        "No valid SEARCH/REPLACE blocks found in patch",
                    ));
                }
                let patcher = crate::patcher::FuzzyBlockPatcher::default();
                let patch_res = match kai_core::traits::CodePatcher::apply_blocks(
                    &patcher,
                    &target_path,
                    &original_content,
                    &blocks,
                ) {
                    Ok(res) => res,
                    Err(err) => {
                        return Ok(ToolResult::error(
                            self.name(),
                            format!("Fuzzy patch application failed: {err}"),
                        ));
                    }
                };
                let summary_msg = format!(
                    "Successfully applied {} block(s) (confidence {:.2}) to '{}' ({} bytes)",
                    patch_res.applied_count,
                    patch_res.confidence_score,
                    target_path.display(),
                    patch_res.modified_content.len()
                );
                (patch_res.modified_content, summary_msg)
            } else {
                let hunks = match Self::parse_unified_diff(&args.patch) {
                    Ok(h) => h,
                    Err(err) => {
                        return Ok(ToolResult::error(
                            self.name(),
                            format!("Diff parse error: {err}"),
                        ));
                    }
                };

                let res_content = match Self::apply_hunks(&original_content, &hunks) {
                    Ok(c) => c,
                    Err(err) => {
                        return Ok(ToolResult::error(
                            self.name(),
                            format!("Patch application failed: {err}"),
                        ));
                    }
                };
                let summary_msg = format!(
                    "Successfully applied {} hunk(s) to '{}' ({} bytes)",
                    hunks.len(),
                    target_path.display(),
                    res_content.len()
                );
                (res_content, summary_msg)
            };

            // Transactional atomic write: write to sibling temp file, then atomic rename
            let parent_dir = target_path.parent().unwrap_or_else(|| Path::new("."));

            if let Err(err) = tokio::fs::create_dir_all(parent_dir).await {
                return Ok(ToolResult::error(
                    self.name(),
                    format!("Failed to create parent directory: {err}"),
                ));
            }

            let file_name = target_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "kai_patch".to_string());

            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);

            let temp_name = format!("{file_name}.tmp.{}.{now}", std::process::id());
            let temp_path = parent_dir.join(temp_name);

            // Write to temporary sibling file with automatic cleanup on failure
            if let Err(err) = tokio::fs::write(&temp_path, patched_content.as_bytes()).await {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Ok(ToolResult::error(
                    self.name(),
                    format!("Failed to write temporary patch file: {err}"),
                ));
            }

            // Perform atomic rename
            if let Err(err) = tokio::fs::rename(&temp_path, &target_path).await {
                // Rollback: remove temporary file
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(KaiError::Tool(ToolError::ExecutionFailed {
                    name: self.name().to_string(),
                    reason: format!("Atomic rename failed: {err}"),
                }));
            }

            Ok(ToolResult::success(self.name(), summary))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kai_core::traits::ToolContext;

    #[tokio::test]
    async fn test_apply_patch_tool_search_replace_block() {
        let unique_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "kai_test_patch_{}_{}",
            std::process::id(),
            unique_id
        ));
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file_path = dir.join("code.rs");
        tokio::fs::write(&file_path, "fn hello() {\n    println!(\"old\");\n}\n")
            .await
            .expect("write");

        let tool = ApplyPatchTool::new();
        let ctx = ToolContext::new(dir.to_path_buf(), "test-session", "test-agent");
        let patch_text = r#"
<<<<<<< SEARCH
    println!("old");
=======
    println!("new");
>>>>>>> REPLACE
"#;
        let args = json!({
            "path": "code.rs",
            "patch": patch_text,
        });

        let res = tool.execute(args, &ctx).await.expect("execute");
        assert!(!res.is_error);

        let modified = tokio::fs::read_to_string(&file_path).await.expect("read");
        assert!(modified.contains("println!(\"new\");"));
        assert!(!modified.contains("println!(\"old\");"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
