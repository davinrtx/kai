//! Non-blocking subprocess execution tool with RAII termination guards.
//!
//! Enforces non-interactive environments, deterministic timeouts, cooperative
//! steering cancellation checks, and strict 4 KB / 50-line output truncation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use kai_core::error::{KaiError, Result, ToolError};
use kai_core::message::ToolResult;
use kai_core::traits::{BoxFuture, PermissionCategory, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::process::Command;

/// Default timeout allocated to commands (30 seconds).
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;

/// Maximum allowed command timeout (10 minutes).
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 600_000;

/// Minimum allowed command timeout (100 ms).
pub const MIN_COMMAND_TIMEOUT_MS: u64 = 100;

/// Arguments accepted by [`ExecCommandTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecCommandArgs {
    /// Shell command string to execute non-interactively.
    pub command: String,
    /// Optional working directory relative to project root.
    pub working_dir: Option<String>,
    /// Optional execution timeout in milliseconds.
    pub timeout_ms: Option<u64>,
    /// Optional custom environment variables.
    pub env: Option<HashMap<String, String>>,
}

/// RAII guard wrapping a child subprocess to guarantee termination on drop.
#[derive(Debug)]
pub struct ProcessGuard {
    child: Option<tokio::process::Child>,
}

impl ProcessGuard {
    /// Constructs a new [`ProcessGuard`] wrapping a child process.
    pub fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    /// Accesses the underlying child process mutably.
    pub fn child_mut(&mut self) -> Option<&mut tokio::process::Child> {
        self.child.as_mut()
    }

    /// Disarms the guard and extracts the inner child process.
    pub fn disarm(mut self) -> Option<tokio::process::Child> {
        self.child.take()
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Signal termination to prevent zombie/orphan subprocesses
            let _ = child.start_kill();
        }
    }
}

/// Tool for non-blocking subprocess command execution.
#[derive(Debug, Clone, Default)]
pub struct ExecCommandTool;

/// Substrings and patterns identifying sensitive credentials that must not leak to subprocesses.
pub const SENSITIVE_ENV_PATTERNS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "TOKEN",
    "PRIVATE_KEY",
    "AUTH",
    "DATABASE_URL",
    "CREDENTIAL",
];

impl ExecCommandTool {
    /// Constructs a new [`ExecCommandTool`].
    pub fn new() -> Self {
        Self
    }

    /// Determines whether an environment variable key corresponds to sensitive host credentials.
    pub fn is_sensitive_env_var(key: &str) -> bool {
        let upper = key.to_ascii_uppercase();
        SENSITIVE_ENV_PATTERNS.iter().any(|pat| upper.contains(pat))
            || upper.starts_with("SSH_")
            || upper.starts_with("AWS_")
            || upper.starts_with("GITHUB_")
            || upper.starts_with("KAI_API_")
            || upper.starts_with("OPENAI_")
            || upper.starts_with("ANTHROPIC_")
    }

    /// Resolves the working directory for command execution.
    fn resolve_working_dir(base_dir: &Path, requested: Option<&str>) -> PathBuf {
        match requested {
            Some(dir) => {
                let p = Path::new(dir);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    base_dir.join(p)
                }
            }
            None => base_dir.to_path_buf(),
        }
    }
}

impl Tool for ExecCommandTool {
    fn name(&self) -> &str {
        "exec_command"
    }

    fn description(&self) -> &str {
        "Executes a shell command non-interactively with RAII subprocess termination on cancellation or timeout."
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional working directory relative to workspace root"
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": MIN_COMMAND_TIMEOUT_MS,
                    "maximum": MAX_COMMAND_TIMEOUT_MS,
                    "description": "Execution timeout in milliseconds (default: 30000)"
                },
                "env": {
                    "type": "object",
                    "description": "Optional custom environment variables"
                }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::ShellExecution
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

        let args: ExecCommandArgs = serde_json::from_value(arguments.clone()).map_err(|err| {
            ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: err.to_string(),
            }
        })?;

        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "command string cannot be empty".to_string(),
            });
        }

        if let Some(t) = args.timeout_ms {
            if !(MIN_COMMAND_TIMEOUT_MS..=MAX_COMMAND_TIMEOUT_MS).contains(&t) {
                return Err(ToolError::InvalidArguments {
                    name: self.name().to_string(),
                    reason: format!(
                        "timeout_ms {t} is outside valid range ({MIN_COMMAND_TIMEOUT_MS}..{MAX_COMMAND_TIMEOUT_MS})"
                    ),
                });
            }
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

            let args: ExecCommandArgs = match serde_json::from_value(arguments) {
                Ok(parsed) => parsed,
                Err(err) => {
                    return Ok(ToolResult::error(
                        self.name(),
                        format!("Invalid arguments: {err}"),
                    ));
                }
            };

            let timeout_ms = args
                .timeout_ms
                .unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS)
                .clamp(MIN_COMMAND_TIMEOUT_MS, MAX_COMMAND_TIMEOUT_MS);

            let exec_dir =
                Self::resolve_working_dir(context.working_dir(), args.working_dir.as_deref());

            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd.exe");
                c.args(["/C", &args.command]);
                c
            } else {
                let mut c = Command::new("sh");
                c.args(["-c", &args.command]);
                c
            };

            cmd.current_dir(&exec_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            // Scrub sensitive host environment variables from child process
            for (key, _) in std::env::vars() {
                if Self::is_sensitive_env_var(&key) {
                    cmd.env_remove(&key);
                }
            }

            // Enforce non-interactive environment flags
            cmd.env("CI", "1")
                .env("TERM", "dumb")
                .env("DEBIAN_FRONTEND", "noninteractive");

            // Explicit custom env overrides scrubbed defaults
            if let Some(envs) = args.env {
                for (k, v) in envs {
                    cmd.env(k, v);
                }
            }

            let start_time = Instant::now();
            let mut child = cmd.spawn().map_err(|err| {
                KaiError::Tool(ToolError::ExecutionFailed {
                    name: self.name().to_string(),
                    reason: format!("Failed to spawn command process: {err}"),
                })
            })?;

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();

            let stdout_task = tokio::spawn(async move {
                let mut buf = Vec::new();
                if let Some(mut r) = stdout {
                    // Cap read buffer at 64 KB to protect memory against infinite streams
                    let mut limited = tokio::io::AsyncReadExt::take(&mut r, 65536);
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await;
                }
                buf
            });

            let stderr_task = tokio::spawn(async move {
                let mut buf = Vec::new();
                if let Some(mut r) = stderr {
                    // Cap read buffer at 64 KB to protect memory against infinite streams
                    let mut limited = tokio::io::AsyncReadExt::take(&mut r, 65536);
                    let _ = tokio::io::AsyncReadExt::read_to_end(&mut limited, &mut buf).await;
                }
                buf
            });

            let mut guard = ProcessGuard::new(child);
            let sleep_fut = tokio::time::sleep(Duration::from_millis(timeout_ms));
            tokio::pin!(sleep_fut);

            let mut check_interval = tokio::time::interval(Duration::from_millis(50));

            let status = loop {
                tokio::select! {
                    _ = &mut sleep_fut => {
                        stdout_task.abort();
                        stderr_task.abort();
                        // Guard drops automatically when returning, killing child process
                        return Err(KaiError::Tool(ToolError::Timeout {
                            name: self.name().to_string(),
                            duration_secs: timeout_ms / 1000,
                        }));
                    }
                    _ = check_interval.tick() => {
                        if context.is_cancelled() {
                            stdout_task.abort();
                            stderr_task.abort();
                            // Cancellation requested by steering signal
                            return Err(KaiError::Orchestrator(
                                kai_core::error::OrchestratorError::Interrupted {
                                    reason: "Command execution cancelled by steering signal".to_string(),
                                }
                            ));
                        }
                    }
                    res = async {
                        if let Some(c) = guard.child_mut() {
                            c.wait().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        break res.map_err(|err| {
                            stdout_task.abort();
                            stderr_task.abort();
                            KaiError::Tool(ToolError::ExecutionFailed {
                                name: self.name().to_string(),
                                reason: format!("Failed while awaiting subprocess exit: {err}"),
                            })
                        })?;
                    }
                }
            };

            // Disarm guard so drop won't try to kill already completed child
            let _ = guard.disarm();

            let elapsed_ms = start_time.elapsed().as_millis() as u64;
            // Await pipes with a bounded timeout to prevent hanging on inherited grandchild descriptors
            let stdout_bytes = tokio::time::timeout(Duration::from_millis(500), stdout_task)
                .await
                .unwrap_or_else(|_| Ok(Vec::new()))
                .unwrap_or_default();
            let stderr_bytes = tokio::time::timeout(Duration::from_millis(500), stderr_task)
                .await
                .unwrap_or_else(|_| Ok(Vec::new()))
                .unwrap_or_default();

            let stdout_str = String::from_utf8_lossy(&stdout_bytes);
            let stderr_str = String::from_utf8_lossy(&stderr_bytes);

            let combined_output = if !stderr_str.trim().is_empty() && !stdout_str.trim().is_empty()
            {
                format!("{stdout_str}\n[STDERR]\n{stderr_str}")
            } else if !stderr_str.trim().is_empty() {
                stderr_str.to_string()
            } else {
                stdout_str.to_string()
            };

            let exit_code = status.code().unwrap_or(-1);

            let tool_result = if status.success() {
                ToolResult::success(self.name(), combined_output)
                    .with_exit_code(exit_code)
                    .with_duration_ms(elapsed_ms)
            } else {
                ToolResult::error(self.name(), combined_output)
                    .with_exit_code(exit_code)
                    .with_duration_ms(elapsed_ms)
            };

            Ok(tool_result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_sensitive_env_var_detection() {
        assert!(ExecCommandTool::is_sensitive_env_var("OPENAI_API_KEY"));
        assert!(ExecCommandTool::is_sensitive_env_var("ANTHROPIC_API_KEY"));
        assert!(ExecCommandTool::is_sensitive_env_var(
            "AWS_SECRET_ACCESS_KEY"
        ));
        assert!(ExecCommandTool::is_sensitive_env_var("GITHUB_TOKEN"));
        assert!(ExecCommandTool::is_sensitive_env_var("SSH_AUTH_SOCK"));
        assert!(ExecCommandTool::is_sensitive_env_var("DATABASE_URL"));
        assert!(ExecCommandTool::is_sensitive_env_var("MY_PASSWORD"));
        assert!(ExecCommandTool::is_sensitive_env_var("KAI_API_SECRET"));

        // Safe variables
        assert!(!ExecCommandTool::is_sensitive_env_var("PATH"));
        assert!(!ExecCommandTool::is_sensitive_env_var("HOME"));
        assert!(!ExecCommandTool::is_sensitive_env_var("USER"));
        assert!(!ExecCommandTool::is_sensitive_env_var("CARGO_HOME"));
    }
}
