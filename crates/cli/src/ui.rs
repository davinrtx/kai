//! Pure-ANSI terminal rendering and interactive user prompt formatting.
//!
//! Provides zero-bloat visual presentation for agent thoughts, tool invocations,
//! diffs, and confirmation prompts without heavyweight terminal TUI dependencies.
//! Automatically enables Windows Virtual Terminal Processing or gracefully disables
//! ANSI codes to prevent escape sequence artifacts (`←[1m`).

use std::io::{self, Write};
use std::path::Path;
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

        if !any_success
            && (std::env::var_os("WT_SESSION").is_some()
                || std::env::var_os("TERM_PROGRAM").is_some()
                || std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false))
        {
            any_success = true;
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

/// Returns `true` if ANSI color output is enabled.
#[inline]
pub fn is_color_enabled() -> bool {
    COLOR_ENABLED.load(Ordering::Relaxed)
}

/// Sets ANSI color output state (primarily for deterministic unit testing).
pub fn set_color_enabled(enabled: bool) {
    COLOR_ENABLED.store(enabled, Ordering::SeqCst);
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

/// Computes visible character count of a string excluding ANSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut count = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else {
            count += 1;
        }
    }
    count
}

/// Returns the active terminal width, bounded between 40 and 120 columns.
pub fn terminal_width() -> usize {
    #[cfg(windows)]
    {
        type HANDLE = *mut std::ffi::c_void;
        type BOOL = i32;
        type SHORT = i16;
        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;

        #[repr(C)]
        struct COORD {
            x: SHORT,
            y: SHORT,
        }
        #[repr(C)]
        struct SMALL_RECT {
            left: SHORT,
            top: SHORT,
            right: SHORT,
            bottom: SHORT,
        }
        #[repr(C)]
        struct CONSOLE_SCREEN_BUFFER_INFO {
            dw_size: COORD,
            dw_cursor_position: COORD,
            w_attributes: u16,
            sr_window: SMALL_RECT,
            dw_maximum_window_size: COORD,
        }

        extern "system" {
            fn GetStdHandle(nStdHandle: u32) -> HANDLE;
            fn GetConsoleScreenBufferInfo(
                hConsoleOutput: HANDLE,
                lpConsoleScreenBufferInfo: *mut CONSOLE_SCREEN_BUFFER_INFO,
            ) -> BOOL;
        }

        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if !handle.is_null() && handle != (-1isize as HANDLE) {
            let mut info = std::mem::MaybeUninit::<CONSOLE_SCREEN_BUFFER_INFO>::uninit();
            let ok = unsafe { GetConsoleScreenBufferInfo(handle, info.as_mut_ptr()) };
            if ok != 0 {
                let info = unsafe { info.assume_init() };
                let width = (info.sr_window.right - info.sr_window.left + 1) as usize;
                if width >= 40 {
                    return width.clamp(40, 120);
                }
            }
        }
    }

    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(w) = cols.parse::<usize>() {
            if w >= 40 {
                return w.clamp(40, 120);
            }
        }
    }

    80
}

/// Splits input text into wrapped lines respecting word boundaries and maximum column bounds.
pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    let mut output = Vec::new();
    for raw_line in text.lines() {
        if raw_line.is_empty() {
            output.push(String::new());
            continue;
        }

        let mut current_line = String::new();
        for word in raw_line.split_whitespace() {
            if current_line.is_empty() {
                if word.len() > max_width {
                    let mut start = 0;
                    while start < word.len() {
                        let end = (start + max_width).min(word.len());
                        output.push(word[start..end].to_string());
                        start = end;
                    }
                } else {
                    current_line.push_str(word);
                }
            } else if current_line.len() + 1 + word.len() <= max_width {
                current_line.push(' ');
                current_line.push_str(word);
            } else {
                output.push(current_line);
                current_line = String::new();
                if word.len() > max_width {
                    let mut start = 0;
                    while start < word.len() {
                        let end = (start + max_width).min(word.len());
                        output.push(word[start..end].to_string());
                        start = end;
                    }
                } else {
                    current_line.push_str(word);
                }
            }
        }
        if !current_line.is_empty() {
            output.push(current_line);
        }
    }

    if output.is_empty() {
        output.push(String::new());
    }

    output
}

/// Draws a stylized box panel with rounded borders, title, and padded content.
pub fn draw_box_panel(
    title: &str,
    lines: &[String],
    width: usize,
    border_color: &str,
    title_color: &str,
) -> String {
    let r = reset();
    let b = bold();

    let safe_width = width.max(20);
    let inner_width = safe_width.saturating_sub(4);

    let mut out = String::new();

    // Top border: ╭─ Title ──...──╮
    if title.is_empty() {
        let dashes = "─".repeat(safe_width.saturating_sub(2));
        out.push_str(&format!("{border_color}╭{dashes}╮{r}\n"));
    } else {
        let title_fmt = format!(" {title} ");
        let title_len = visible_width(title) + 2;
        let right_dashes = safe_width.saturating_sub(3 + title_len);
        let dashes = "─".repeat(right_dashes);
        out.push_str(&format!(
            "{border_color}╭─{r}{b}{title_color}{title_fmt}{r}{border_color}{dashes}╮{r}\n"
        ));
    }

    // Body lines: │  content  │
    for line in lines {
        let char_count = visible_width(line);
        let padding = inner_width.saturating_sub(char_count);
        let spaces = " ".repeat(padding);
        out.push_str(&format!(
            "{border_color}│{r}  {line}{spaces}  {border_color}│{r}\n"
        ));
    }

    // Bottom border: ╰───────...───────╯
    let bottom_dashes = "─".repeat(safe_width.saturating_sub(2));
    out.push_str(&format!("{border_color}╰{bottom_dashes}╯{r}"));

    out
}

/// Formats a path to display cleanly inside narrow banners.
fn format_short_path(path: &Path, max_len: usize) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.len() <= max_len {
        s
    } else if max_len <= 3 {
        "...".to_string()
    } else {
        format!("...{}", &s[s.len() - (max_len - 3)..])
    }
}

/// Prints the stylized KAI welcome banner with a two-column grid.
pub fn print_banner(
    version: &str,
    model: &str,
    base_url: &str,
    working_dir: &Path,
    session_id: &str,
    tool_names: &[String],
    skills_count: usize,
) {
    let width = terminal_width().clamp(72, 110);
    let border = yellow();
    let accent = cyan();
    let d = dim();
    let r = reset();
    let b = bold();
    let rd = red();

    let title = format!("KAI (Krill Agent Interface) v{version}");
    let is_unconfigured = model == crate::config::UNCONFIGURED_MODEL || model.is_empty();

    if width >= 72 {
        // Two-column layout
        let left_col = 32;
        let left_inner_box = left_col + 4;
        let right_inner_box = width.saturating_sub(3 + left_inner_box);
        let right_col = right_inner_box.saturating_sub(4);

        // Left hero emblem
        let hero_art = [
            "          /\\",
            "         /  \\",
            "        / /\\ \\",
            "       / / ◈\\ \\",
            "       \\ \\   / /",
            "        \\ \\ / /",
            "         \\ V /",
        ];

        let short_dir = format_short_path(working_dir, 22);
        let short_sess = if session_id.len() > 22 {
            format!("{}...", &session_id[..19])
        } else {
            session_id.to_string()
        };

        let mut left_lines = Vec::new();
        for art in hero_art {
            left_lines.push(format!("{art:<left_col$}"));
        }
        left_lines.push(String::new());

        if is_unconfigured {
            left_lines.push(format!("{d}Model:{r} {b}{rd}no model configured{r}"));
            left_lines.push(format!("{d}Hint:{r}  {d}run /model to set{r}"));
        } else {
            let short_model = if model.len() > 22 {
                format!("{}...", &model[..19])
            } else {
                model.to_string()
            };
            left_lines.push(format!("{d}Model:{r} {b}{short_model}{r}"));
            left_lines.push(format!(
                "{d}Host:{r}  {}",
                format_short_path(Path::new(base_url), 22)
            ));
        }
        left_lines.push(format!("{d}Dir:{r}   {short_dir}"));
        left_lines.push(format!("{d}Sess:{r}  {short_sess}"));

        // Right column: tools, skills, summary
        let mut right_lines = Vec::new();
        right_lines.push(format!("{b}{accent}Available Tools{r}"));

        let mut fs_tools = Vec::new();
        let mut exec_tools = Vec::new();
        let mut agent_tools = Vec::new();
        let mut other_tools = Vec::new();

        for name in tool_names {
            match name.as_str() {
                "read_window" | "apply_patch" => fs_tools.push(name.as_str()),
                "exec_command" => exec_tools.push(name.as_str()),
                "delegate_task" | "clarify" => agent_tools.push(name.as_str()),
                _ => other_tools.push(name.as_str()),
            }
        }

        if !fs_tools.is_empty() {
            right_lines.push(format!("{d}fs:{r}     {}", fs_tools.join(", ")));
        }
        if !exec_tools.is_empty() {
            right_lines.push(format!("{d}exec:{r}   {}", exec_tools.join(", ")));
        }
        if !agent_tools.is_empty() {
            right_lines.push(format!("{d}agent:{r}  {}", agent_tools.join(", ")));
        }
        if !other_tools.is_empty() {
            right_lines.push(format!("{d}core:{r}   {}", other_tools.join(", ")));
        }

        right_lines.push(String::new());
        right_lines.push(format!("{b}{accent}Available Skills{r}"));
        if skills_count > 0 {
            right_lines.push(format!("{d}catalog:{r} {skills_count} procedural skill(s)"));
        } else {
            right_lines.push(format!("{d}catalog:{r} 0 procedural skills"));
        }

        right_lines.push(String::new());
        right_lines.push(format!(
            "{d}{} tools · {} skills · /help for commands{r}",
            tool_names.len(),
            skills_count
        ));

        // Equalize line counts
        let max_rows = left_lines.len().max(right_lines.len());
        while left_lines.len() < max_rows {
            left_lines.push(String::new());
        }
        while right_lines.len() < max_rows {
            right_lines.push(String::new());
        }

        // Render combined box
        let title_fmt = format!(" {title} ");
        let title_len = visible_width(&title) + 2;
        let right_dashes = width.saturating_sub(3 + title_len);
        println!(
            "{border}╭─{r}{b}{accent}{title_fmt}{r}{border}{}╮{r}",
            "─".repeat(right_dashes)
        );

        for i in 0..max_rows {
            let left = &left_lines[i];
            let right = &right_lines[i];

            let left_vis = visible_width(left);
            let right_vis = visible_width(right);

            let left_pad = " ".repeat(left_col.saturating_sub(left_vis));
            let right_pad = " ".repeat(right_col.saturating_sub(right_vis));

            println!(
                "{border}│{r}  {left}{left_pad}  {border}│{r}  {right}{right_pad}  {border}│{r}"
            );
        }

        let bot_left = "─".repeat(left_inner_box);
        let bot_right = "─".repeat(right_inner_box);
        println!("{border}╰{bot_left}┴{bot_right}╯{r}\n");
    } else {
        // Narrow terminal stacked layout
        let mut lines = Vec::new();
        if is_unconfigured {
            lines.push(format!("{b}{rd}no model configured{r} {d}— run /model{r}"));
        } else {
            lines.push(format!("{b}Model:{r} {model}"));
            lines.push(format!("{d}Endpoint:{r} {base_url}"));
        }
        lines.push(format!("{d}Working Dir:{r} {}", working_dir.display()));
        lines.push(format!(
            "{d}Tools: {}{r} | {d}Skills: {}{r}",
            tool_names.len(),
            skills_count
        ));
        lines.push(format!("{d}Type '/help' for command manual{r}"));

        let panel = draw_box_panel(&title, &lines, width, border, accent);
        println!("{panel}\n");
    }
}

/// Detects the currently checked-out Git branch from the repository containing `dir`.
pub fn detect_git_branch(dir: &Path) -> Option<String> {
    let mut current = dir.to_path_buf();
    for _ in 0..10 {
        let git_path = current.join(".git");
        if git_path.is_dir() {
            let head_path = git_path.join("HEAD");
            if let Ok(content) = std::fs::read_to_string(&head_path) {
                let trimmed = content.trim();
                if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                    return Some(branch.to_string());
                } else if trimmed.len() >= 7 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(trimmed[..7].to_string());
                }
            }
            return None;
        } else if git_path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&git_path) {
                let trimmed = content.trim();
                if let Some(gitdir_rel) = trimmed.strip_prefix("gitdir:") {
                    let gitdir = current.join(gitdir_rel.trim());
                    let head_path = gitdir.join("HEAD");
                    if let Ok(head_content) = std::fs::read_to_string(&head_path) {
                        let trimmed_head = head_content.trim();
                        if let Some(branch) = trimmed_head.strip_prefix("ref: refs/heads/") {
                            return Some(branch.to_string());
                        } else if trimmed_head.len() >= 7
                            && trimmed_head.chars().all(|c| c.is_ascii_hexdigit())
                        {
                            return Some(trimmed_head[..7].to_string());
                        }
                    }
                }
            }
            return None;
        }

        if !current.pop() {
            break;
        }
    }
    None
}

/// Separates `<think>...</think>` internal reasoning blocks from the assistant's final text.
pub fn parse_reasoning_blocks(text: &str) -> (Option<String>, String) {
    const START_TAG: &str = "<think>";
    const END_TAG: &str = "</think>";

    if !text.contains(START_TAG) {
        return (None, text.to_string());
    }

    let mut thoughts = Vec::new();
    let mut response_clean = String::new();
    let mut cursor = 0;

    while let Some(start_idx) = text[cursor..].find(START_TAG) {
        let actual_start = cursor + start_idx;
        response_clean.push_str(&text[cursor..actual_start]);

        let after_start = actual_start + START_TAG.len();
        if let Some(end_idx) = text[after_start..].find(END_TAG) {
            let actual_end = after_start + end_idx;
            let thought_segment = text[after_start..actual_end].trim();
            if !thought_segment.is_empty() {
                thoughts.push(thought_segment);
            }
            cursor = actual_end + END_TAG.len();
        } else {
            let thought_segment = text[after_start..].trim();
            if !thought_segment.is_empty() {
                thoughts.push(thought_segment);
            }
            cursor = text.len();
            break;
        }
    }

    if cursor < text.len() {
        response_clean.push_str(&text[cursor..]);
    }

    let thought_opt = if thoughts.is_empty() {
        None
    } else {
        Some(thoughts.join("\n\n"))
    };

    (thought_opt, response_clean.trim().to_string())
}

/// Renders the turn-based status bar reporting active runtime metadata.
pub fn print_status_bar(
    model: &str,
    tokens: usize,
    git_branch: Option<&str>,
    session_branch: &str,
) {
    let b = bold();
    let d = dim();
    let c = cyan();
    let g = green();
    let y = yellow();
    let m = magenta();
    let rd = red();
    let r = reset();

    let git_display = git_branch.unwrap_or("none");
    let token_str = if tokens >= 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{tokens}")
    };

    let model_display = if model == crate::config::UNCONFIGURED_MODEL || model.is_empty() {
        format!("{b}{rd}[no model]{r}")
    } else {
        format!("{b}{model}{r}")
    };

    println!(
        "\n  {model_display} {d}·{r} {y}{token_str} tokens{r} {d}·{r} {g}⎇ {git_display}{r} {d}·{r} {m}{session_branch}{r} {d}·{r} {c}[Ready]{r}"
    );
}

/// Formats and prints a user prompt confirmation divider.
pub fn print_user_prompt_preview(text: &str) {
    let width = terminal_width().min(60);
    let d = dim();
    let b = bold();
    let r = reset();

    let line = "─".repeat(width.saturating_sub(4));
    println!("  {d}{line}{r}");
    println!("  {b}●{r} {}\n", text.trim());
}

/// Formats and prints an agent's reasoning trace inside a dedicated box panel.
pub fn print_thought(text: &str) {
    let d = dim();
    let r = reset();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }

    let width = terminal_width().clamp(50, 100);
    let inner_width = width.saturating_sub(6);

    let wrapped = wrap_text(trimmed, inner_width);
    let total_lines = wrapped.len();

    let title = " Reasoning ";
    let top_dashes = width.saturating_sub(4 + title.len());
    println!("{d}┌─{title}{}{r}", "─".repeat(top_dashes));

    let display_limit = 10;
    for line in wrapped.iter().take(display_limit) {
        println!("{d}│{r}  {d}{line}{r}");
    }

    if total_lines > display_limit {
        let remaining = total_lines - display_limit;
        println!(
            "{d}│  ... ({remaining} more lines omitted — type '/reasoning on' to view full trace){r}"
        );
    }

    let bot_dashes = "─".repeat(width.saturating_sub(2));
    println!("{d}└{bot_dashes}┘{r}");
}

/// Formats and prints a tool invocation notification with tree prefix.
pub fn print_tool_invocation(name: &str, args: &Value) {
    let b = bold();
    let c = cyan();
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

    println!("  {d}┊{r} {b}{c}◈ {name}{r}  {d}({args_summary}){r}");
}

/// Formats and prints a tool execution result with tree prefix.
pub fn print_tool_result(name: &str, is_error: bool, output: &str) {
    let b = bold();
    let g = green();
    let rd = red();
    let d = dim();
    let r = reset();

    if is_error {
        let first_line = output.lines().next().unwrap_or("execution failed");
        let short_err = if first_line.len() > 60 {
            format!("{}...", &first_line[..57])
        } else {
            first_line.to_string()
        };
        println!("  {d}┊{r} {b}{rd}✗ {name}{r}  {rd}{short_err}{r}");
    } else {
        let first_line = output.lines().next().unwrap_or("completed");
        let short_out = if first_line.len() > 60 {
            format!("{}...", &first_line[..57])
        } else {
            first_line.to_string()
        };
        println!("  {d}┊{r} {b}{g}✓ {name}{r}  {d}{short_out}{r}");
    }
}

/// Formats and prints the assistant response inside a stylized rounded panel.
pub fn print_assistant_response(text: &str) {
    let width = terminal_width().clamp(50, 110);
    let inner_width = width.saturating_sub(6);

    let wrapped = wrap_text(text, inner_width);

    let panel = draw_box_panel("◈ KAI", &wrapped, width, magenta(), cyan());
    println!("\n{panel}\n");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_wrapping() {
        let text = "The quick brown fox jumps over the lazy dog";
        let wrapped = wrap_text(text, 15);
        assert!(wrapped.len() >= 3);
        for line in &wrapped {
            assert!(line.len() <= 15);
        }
    }

    #[test]
    fn test_draw_box_panel() {
        set_color_enabled(false);
        let content = vec!["Line 1".to_string(), "Line 2".to_string()];
        let panel = draw_box_panel("Test", &content, 30, "", "");
        assert!(panel.contains("Test"));
        assert!(panel.contains("Line 1"));
        assert!(panel.contains("Line 2"));
        assert!(panel.contains('╭'));
        assert!(panel.contains('╰'));
    }

    #[test]
    fn test_parse_reasoning_blocks() {
        let raw = "<think>Analyzing context tree</think>Here is the solution.";
        let (thought, clean) = parse_reasoning_blocks(raw);
        assert_eq!(thought.as_deref(), Some("Analyzing context tree"));
        assert_eq!(clean, "Here is the solution.");
    }

    #[test]
    fn test_visible_width() {
        assert_eq!(visible_width("hello world"), 11);
        assert_eq!(visible_width("\x1b[1m\x1b[31mno model\x1b[0m"), 8);
        assert_eq!(visible_width("\x1b[32m⎇ feat/cli\x1b[0m"), 10);
    }
}
