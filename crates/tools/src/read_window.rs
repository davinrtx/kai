//! Bounded windowed file reader tool.
//!
//! Enforces a strict maximum limit of 150 lines per call and formats lines with
//! 1-based line numbers. Designed for low-RAM footprints and bounded context windows.

use std::path::{Path, PathBuf};

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Maximum number of lines permitted per single `read_window` invocation.
pub const MAX_WINDOW_LIMIT: usize = 150;

/// Default number of lines returned when `limit` is not specified.
pub const DEFAULT_WINDOW_LIMIT: usize = 50;

/// Arguments accepted by the [`ReadWindowTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadWindowArgs {
    /// Relative or absolute path to the target file.
    pub path: String,
    /// 1-based line offset where reading starts (default: 1).
    #[serde(default = "default_offset")]
    pub offset: usize,
    /// Maximum number of lines to return (default: 50, maximum: 150).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_offset() -> usize {
    1
}

fn default_limit() -> usize {
    DEFAULT_WINDOW_LIMIT
}

/// Tool for reading a bounded window of lines from a file.
#[derive(Debug, Clone, Default)]
pub struct ReadWindowTool;

impl ReadWindowTool {
    /// Constructs a new [`ReadWindowTool`].
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
}

impl Tool for ReadWindowTool {
    fn name(&self) -> &str {
        "read_window"
    }

    fn description(&self) -> &str {
        "Reads a bounded window of lines from a file, strictly enforcing a maximum limit of 150 lines with 1-based line numbers."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path of the file to read"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based starting line number (default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 150,
                    "description": "Number of lines to read (default: 50, max: 150)"
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
        self.default_validate_arguments(arguments)?;

        let args: ReadWindowArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        if args.limit == 0 || args.limit > MAX_WINDOW_LIMIT {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: format!(
                    "limit {} must be between 1 and {}",
                    args.limit, MAX_WINDOW_LIMIT
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

            let args: ReadWindowArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            if args.limit == 0 || args.limit > MAX_WINDOW_LIMIT {
                return Ok(ToolResult::error(
                    self.name(),
                    format!(
                        "Invalid limit: {} must be between 1 and {}",
                        args.limit, MAX_WINDOW_LIMIT
                    ),
                ));
            }

            let start_line = if args.offset == 0 { 1 } else { args.offset };
            let target_path = Self::resolve_path(context.working_dir(), &args.path);

            if !target_path.exists() {
                return Ok(ToolResult::error(
                    self.name(),
                    format!("File not found: {}", target_path.display()),
                ));
            }

            if target_path.is_dir() {
                return Ok(ToolResult::error(
                    self.name(),
                    format!("Target path is a directory: {}", target_path.display()),
                ));
            }

            let file = match tokio::fs::File::open(&target_path).await {
                Ok(f) => f,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Failed to open file: {err}"),
                    ));
                }
            };

            let reader = BufReader::new(file);
            let mut lines = reader.lines();
            let mut current_line_no = 0usize;
            let mut output_buffer = String::new();
            let mut lines_read = 0usize;
            let mut has_more = false;

            while let Some(line) = lines.next_line().await.map_err(|err| {
                KaiError::Tool(ToolError::ExecutionFailed {
                    name: self.name().to_string(),
                    reason: format!("I/O error reading file: {err}"),
                })
            })? {
                current_line_no += 1;

                if current_line_no % 1024 == 0 {
                    context.check_cancellation()?;
                }

                if current_line_no < start_line {
                    continue;
                }

                if lines_read < args.limit {
                    let display_line = if line.len() > 2048 {
                        let mut boundary = 2048;
                        while boundary > 0 && !line.is_char_boundary(boundary) {
                            boundary -= 1;
                        }
                        format!("{} [Line truncated: exceeded 2KB cap]", &line[..boundary])
                    } else {
                        line
                    };
                    output_buffer.push_str(&format!("{:>6} | {}\n", current_line_no, display_line));
                    lines_read += 1;
                } else {
                    has_more = true;
                    break;
                }
            }

            if lines_read == 0 && current_line_no < start_line {
                return Ok(ToolResult::success(
                    self.name(),
                    format!(
                        "[Offset {} beyond end of file (total lines: {})]",
                        start_line, current_line_no
                    ),
                ));
            }

            if has_more {
                output_buffer.push_str(&format!(
                    "\n[Window ended at line {}. More lines available in file]",
                    start_line + lines_read - 1
                ));
            }

            Ok(ToolResult::success(self.name(), output_buffer))
        })
    }
}

impl ReadWindowTool {
    fn default_validate_arguments(&self, arguments: &serde_json::Value) -> Result<(), ToolError> {
        if arguments.is_object() {
            Ok(())
        } else {
            Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "Arguments must be a valid JSON object".to_string(),
            })
        }
    }
}
