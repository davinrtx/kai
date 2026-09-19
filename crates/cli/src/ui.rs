//! Pure-ANSI terminal rendering and interactive user prompt formatting.
//!
//! Provides zero-bloat visual presentation for agent thoughts, tool invocations,
//! diffs, and confirmation prompts without heavyweight terminal TUI dependencies.

use std::io::{self, Write};

use kai_core::traits::{ApprovalDecision, ToolApprovalPolicy, ToolContext};
use serde_json::Value;

// ANSI escape sequences
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

/// Prints the KAI CLI ASCII banner and runtime metadata.
pub fn print_banner(version: &str, model: &str, base_url: &str) {
    println!("{BOLD}{CYAN}=== KAI (Krill Agent Interface) v{version} ==={RESET}");
    println!("{DIM}Endpoint: {base_url} | Model: {model}{RESET}\n");
}

/// Formats and prints an agent's reasoning trace or thought.
pub fn print_thought(text: &str) {
    if !text.trim().is_empty() {
        println!("{CYAN}{DIM}> {text}{RESET}");
    }
}

/// Formats and prints a tool invocation notification.
pub fn print_tool_invocation(name: &str, args: &Value) {
    let args_summary = if let Value::Object(map) = args {
        let pairs: Vec<String> = map
            .iter()
            .take(3)
            .map(|(k, v)| {
                let s = v.to_string();
                let truncated = if s.len() > 30 {
                    format!("{}...", &s[..27])
                } else {
                    s
                };
                format!("{k}={truncated}")
            })
            .collect();
        pairs.join(", ")
    } else {
        String::new()
    };

    println!("{BOLD}{YELLOW}[Tool Call]{RESET} {BOLD}{name}{RESET} {DIM}({args_summary}){RESET}");
}

/// Formats and prints a tool execution result.
pub fn print_tool_result(name: &str, is_error: bool, output: &str) {
    if is_error {
        println!("{BOLD}{RED}[Tool Error: {name}]{RESET}\n{RED}{output}{RESET}");
    } else {
        let first_lines: Vec<&str> = output.lines().take(3).collect();
        let summary = first_lines.join("\n");
        println!("{BOLD}{GREEN}[Tool Result: {name}]{RESET}\n{DIM}{summary}{RESET}");
        if output.lines().count() > 3 {
            println!(
                "{DIM}... ({} lines omitted){RESET}",
                output.lines().count() - 3
            );
        }
    }
}

/// Formats and prints the final assistant message.
pub fn print_assistant_response(text: &str) {
    println!("\n{BOLD}{MAGENTA}kai>{RESET} {text}\n");
}

/// Formats and prints an error message.
pub fn print_error(msg: &str) {
    eprintln!("{BOLD}{RED}Error:{RESET} {msg}");
}

/// Formats and prints an informational notice.
pub fn print_info(msg: &str) {
    println!("{BOLD}{CYAN}Info:{RESET} {msg}");
}

/// Interactive CLI approval policy for evaluating tool execution authorization.
pub struct CliApprovalPolicy {
    auto_approve: bool,
}

impl CliApprovalPolicy {
    /// Constructs a new [`CliApprovalPolicy`].
    pub fn new(auto_approve: bool) -> Self {
        Self { auto_approve }
    }
}

impl ToolApprovalPolicy for CliApprovalPolicy {
    fn evaluate(
        &self,
        tool_name: &str,
        arguments: &Value,
        _context: &ToolContext,
    ) -> ApprovalDecision {
        if self.auto_approve {
            return ApprovalDecision::Approved;
        }

        // Tools that write files or execute commands trigger confirmation
        match tool_name {
            "exec_command" => {
                let cmd = arguments
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("<unknown>");
                let reason = format!("Subprocess command execution: `{cmd}`");
                if prompt_user_confirmation(&reason) {
                    ApprovalDecision::Approved
                } else {
                    ApprovalDecision::Denied {
                        reason: "User declined command execution".to_string(),
                    }
                }
            }
            "apply_patch" => {
                let path = arguments
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("<unknown>");
                let reason = format!("Transactional modification to file: `{path}`");
                if prompt_user_confirmation(&reason) {
                    ApprovalDecision::Approved
                } else {
                    ApprovalDecision::Denied {
                        reason: "User declined file modification".to_string(),
                    }
                }
            }
            _ => ApprovalDecision::Approved,
        }
    }
}

/// Prompts the user on STDIN to confirm a suspended tool call.
pub fn prompt_user_confirmation(reason: &str) -> bool {
    print!("{BOLD}{YELLOW}[Confirmation Required]{RESET} {reason}\nProceed? [y/N]: ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim().to_lowercase();
        trimmed == "y" || trimmed == "yes"
    } else {
        false
    }
}
