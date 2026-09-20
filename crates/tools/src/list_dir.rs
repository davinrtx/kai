//! Bounded directory listing tool.
//!
//! Strictly enforces maximum limits of 50 items per call and formats entries with
//! directory indicators and file sizes. Designed for low-RAM footprints and bounded context windows.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Maximum number of items permitted per single `list_dir` invocation.
pub const MAX_LIST_DIR_LIMIT: usize = 50;

/// Default number of items returned when `limit` is not specified.
pub const DEFAULT_LIST_DIR_LIMIT: usize = 50;

/// Arguments accepted by the [`ListDirTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirArgs {
    /// Relative or absolute path to the target directory (default: ".").
    #[serde(default = "default_path")]
    pub path: String,
    /// 0-based offset where entry listing starts (default: 0).
    #[serde(default = "default_offset")]
    pub offset: usize,
    /// Maximum number of entries to return (default: 50, maximum: 50).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_path() -> String {
    ".".to_string()
}

fn default_offset() -> usize {
    0
}

fn default_limit() -> usize {
    DEFAULT_LIST_DIR_LIMIT
}

/// Entry item metadata collected during directory scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DirEntryItem {
    name: String,
    is_dir: bool,
    size_bytes: u64,
}

impl Ord for DirEntryItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // Directories first, then alphabetical by name (case-insensitive)
        match (self.is_dir, other.is_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => self
                .name
                .to_lowercase()
                .cmp(&other.name.to_lowercase())
                .then_with(|| self.name.cmp(&other.name)),
        }
    }
}

impl PartialOrd for DirEntryItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Tool for listing contents of a directory in a bounded and deterministic manner.
#[derive(Debug, Clone, Default)]
pub struct ListDirTool;

impl ListDirTool {
    /// Constructs a new [`ListDirTool`].
    pub fn new() -> Self {
        Self
    }

    /// Resolves the directory path relative to the active working directory if relative.
    fn resolve_path(base_dir: &Path, requested: &str) -> PathBuf {
        let trimmed = requested.trim();
        let target = if trimmed.is_empty() || trimmed == "." {
            Path::new(".")
        } else {
            Path::new(trimmed)
        };

        if target.is_absolute() {
            target.to_path_buf()
        } else {
            base_dir.join(target)
        }
    }
}

impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "Lists contents of a directory (files and subdirectories) with bounded entries, file sizes, and directory markers."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path of the directory to list (default: current working directory \".\")"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "0-based index offset for pagination (default: 0)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Maximum number of entries to return (default: 50, max: 50)"
                }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileRead
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if !arguments.is_object() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object".to_string(),
            });
        }

        let args: ListDirArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        if args.limit == 0 || args.limit > MAX_LIST_DIR_LIMIT {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: format!(
                    "limit {} must be between 1 and {}",
                    args.limit, MAX_LIST_DIR_LIMIT
                ),
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

            let args: ListDirArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            if args.limit == 0 || args.limit > MAX_LIST_DIR_LIMIT {
                return Ok(ToolResult::error(
                    self.name(),
                    format!(
                        "Invalid limit: {} must be between 1 and {}",
                        args.limit, MAX_LIST_DIR_LIMIT
                    ),
                ));
            }

            let target_path = Self::resolve_path(context.working_dir(), &args.path);

            if !target_path.exists() {
                return Ok(ToolResult::error(
                    self.name(),
                    format!("Directory not found: {}", target_path.display()),
                ));
            }

            if !target_path.is_dir() {
                return Ok(ToolResult::error(
                    self.name(),
                    format!(
                        "Path is a file, not a directory: {}. Use 'read_window' to read file contents.",
                        target_path.display()
                    ),
                ));
            }

            // Read directory entries asynchronously
            let mut read_dir = match tokio::fs::read_dir(&target_path).await {
                Ok(reader) => reader,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Failed to read directory {}: {err}", target_path.display()),
                    ));
                }
            };

            let mut entries = Vec::new();
            while let Some(entry) = read_dir.next_entry().await.map_err(KaiError::Io)? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let file_type = entry.file_type().await.map_err(KaiError::Io)?;
                let is_dir = file_type.is_dir();

                let size_bytes = if is_dir {
                    0
                } else {
                    entry.metadata().await.map(|m| m.len()).unwrap_or(0)
                };

                entries.push(DirEntryItem {
                    name,
                    is_dir,
                    size_bytes,
                });
            }

            // Deterministic sorting
            entries.sort();

            let total_entries = entries.len();
            if total_entries == 0 {
                return Ok(ToolResult::success(
                    self.name(),
                    format!("Directory {} is empty (0 items).", target_path.display()),
                ));
            }

            let start_offset = args.offset.min(total_entries);
            let end_offset = (start_offset + args.limit).min(total_entries);
            let paged_entries = &entries[start_offset..end_offset];
            let remaining = total_entries.saturating_sub(end_offset);

            let mut output = format!(
                "Directory: {} (showing entries {}-{} of {total_entries})\n",
                target_path.display(),
                if total_entries == 0 {
                    0
                } else {
                    start_offset + 1
                },
                end_offset
            );

            for item in paged_entries {
                if item.is_dir {
                    output.push_str(&format!("[DIR]  {}/\n", item.name));
                } else {
                    output.push_str(&format!(
                        "[FILE] {} ({} bytes)\n",
                        item.name, item.size_bytes
                    ));
                }
            }

            if remaining > 0 {
                output.push_str(&format!(
                    "\n[Truncated: {remaining} remaining items. Use 'offset' to view more entries]\n"
                ));
            }

            Ok(ToolResult::success(self.name(), output))
        })
    }
}
