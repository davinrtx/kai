//! Pure-ANSI terminal rendering and interactive user prompt formatting.
//!
//! Provides zero-bloat visual presentation for agent thoughts, tool invocations,
//! diffs, and confirmation prompts without heavyweight terminal TUI dependencies.
//! Automatically enables Windows Virtual Terminal Processing or gracefully disables
//! ANSI codes to prevent escape sequence artifacts (`←[1m`).

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use kai_core::traits::{ApprovalDecision, ToolApprovalPolicy, ToolContext};
use serde_json::Value;

static COLOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Initializes console subsystem and enables Virtual Terminal Processing if supported.
pub fn init_terminal() {
    if std::env::var_os("NO_COLOR").is_some() {
        COLOR_ENABLED.store(false, Ordering::SeqCst);
        return;
    }

    #[cfg(windows)]
    {
        type HANDLE = *mut std::ffi::c_void;
        type BOOL = i32;
        type DWORD = u32;

        const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
        const STD_ERROR_HANDLE: DWORD = -12i32 as DWORD;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;

        extern "system" {
            fn GetStdHandle(nStdHandle: DWORD) -> HANDLE;
            fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut DWORD) -> BOOL;
            fn SetConsoleMode(hConsoleHandle: HANDLE, dwMode: DWORD) -> BOOL;
        }

        // Safety Invariant:
        // C-FFI call to standard Win32 console functions with stack pointers.
        // Verifies handle is non-null and not INVALID_HANDLE_VALUE (-1).
        let mut any_success = false;
        for handle_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = unsafe { GetStdHandle(handle_id) };
            if !handle.is_null() && handle != (-1isize as HANDLE) {
                let mut mode: DWORD = 0;
                let ok = unsafe {
                    if GetConsoleMode(handle, &mut mode) != 0 {
                        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
                    } else {
                        false
                    }
                };
                if ok {
                    any_success = true;
                }
            }
        }
        COLOR_ENABLED.store(any_success, Ordering::SeqCst);
    }

    #[cfg(not(windows))]
    {
        let term = std::env::var("TERM").unwrap_or_default();
        let supported = term != "dumb";
        COLOR_ENABLED.store(supported, Ordering::SeqCst);
    }
}

/// Returns the ANSI reset code if colors are enabled, or empty string.
#[inline]
pub fn reset() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[0m"
    } else {
        ""
    }
}

/// Returns the ANSI bold code if colors are enabled, or empty string.
#[inline]
pub fn bold() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[1m"
    } else {
        ""
    }
}

/// Returns the ANSI dim code if colors are enabled, or empty string.
#[inline]
pub fn dim() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[2m"
    } else {
        ""
    }
}

/// Returns the ANSI green code if colors are enabled, or empty string.
#[inline]
pub fn green() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[32m"
    } else {
        ""
    }
}

/// Returns the ANSI yellow code if colors are enabled, or empty string.
#[inline]
pub fn yellow() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[33m"
    } else {
        ""
    }
}

/// Returns the ANSI cyan code if colors are enabled, or empty string.
#[inline]
pub fn cyan() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[36m"
    } else {
        ""
    }
}

/// Returns the ANSI red code if colors are enabled, or empty string.
#[inline]
pub fn red() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[31m"
    } else {
        ""
    }
}

/// Returns the ANSI magenta code if colors are enabled, or empty string.
#[inline]
pub fn magenta() -> &'static str {
    if COLOR_ENABLED.load(Ordering::Relaxed) {
        "\x1b[35m"
    } else {
        ""
    }
}

/// Prints the KAI CLI ASCII banner and runtime metadata.
pub fn print_banner(version: &str, model: &str, base_url: &str) {
    let b = bold();
    let c = cyan();
    let d = dim();
    let r = reset();

    println!("{b}{c}=== KAI (Krill Agent Interface) v{version} ==={r}");
    println!("{d}Endpoint: {base_url} | Model: {model}{r}\n");
}

/// Formats and prints an agent's reasoning trace or thought.
pub fn print_thought(text: &str) {
    let c = cyan();
    let d = dim();
    let r = reset();
    if !text.trim().is_empty() {
        println!("{c}{d}> {text}{r}");
    }
}

/// Formats and prints a tool invocation notification.
pub fn print_tool_invocation(name: &str, args: &Value) {
    let b = bold();
    let y = yellow();
    let d = dim();
    let r = reset();

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

    println!("{b}{y}[Tool Call]{r} {b}{name}{r} {d}({args_summary}){r}");
}

/// Formats and prints a tool execution result.
pub fn print_tool_result(name: &str, is_error: bool, output: &str) {
    let b = bold();
    let g = green();
    let rd = red();
    let d = dim();
    let r = reset();

    if is_error {
        println!("{b}{rd}[Tool Error: {name}]{r}\n{rd}{output}{r}");
    } else {
        let first_lines: Vec<&str> = output.lines().take(3).collect();
        let summary = first_lines.join("\n");
        println!("{b}{g}[Tool Result: {name}]{r}\n{d}{summary}{r}");
        if output.lines().count() > 3 {
            println!("{d}... ({} lines omitted){r}", output.lines().count() - 3);
        }
    }
}

/// Formats and prints the final assistant message.
pub fn print_assistant_response(text: &str) {
    let b = bold();
    let m = magenta();
    let r = reset();
    println!("\n{b}{m}kai>{r} {text}\n");
}

/// Formats and prints an error message.
pub fn print_error(msg: &str) {
    let b = bold();
    let rd = red();
    let r = reset();
    eprintln!("{b}{rd}Error:{r} {msg}");
}

/// Formats and prints an informational notice.
pub fn print_info(msg: &str) {
    let b = bold();
    let c = cyan();
    let r = reset();
    println!("{b}{c}Info:{r} {msg}");
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
    let b = bold();
    let y = yellow();
    let r = reset();
    print!("{b}{y}[Confirmation Required]{r} {reason}\nProceed? [y/N]: ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_ok() {
        let trimmed = input.trim().to_lowercase();
        trimmed == "y" || trimmed == "yes"
    } else {
        false
    }
}
