//! Pure-ANSI terminal rendering and interactive user prompt formatting.
//!
//! Provides zero-bloat visual presentation for agent thoughts, tool invocations,
//! diffs, and confirmation prompts without heavyweight terminal TUI dependencies.
//! Automatically enables Windows Virtual Terminal Processing or gracefully disables
//! ANSI codes to prevent escape sequence artifacts (`←[1m`).

use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use kai_core::traits::{ApprovalDecision, ToolApprovalPolicy, ToolContext};
use serde_json::Value;

static COLOR_ENABLED: AtomicBool = AtomicBool::new(false);

/// Global OS signal subsystem providing reliable, non-terminating Ctrl+C cancellation.
pub mod signal {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use tokio::sync::broadcast;

    static NOTIFY_TX: OnceLock<broadcast::Sender<()>> = OnceLock::new();
    static WAS_CANCELLED: AtomicBool = AtomicBool::new(false);

    #[cfg(windows)]
    mod win {
        use super::*;
        type BOOL = i32;
        type DWORD = u32;

        const STD_INPUT_HANDLE: DWORD = -10i32 as DWORD;
        const ENABLE_PROCESSED_INPUT: DWORD = 0x0001;

        extern "system" {
            fn GetStdHandle(nStdHandle: DWORD) -> *mut std::ffi::c_void;
            fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut DWORD) -> BOOL;
            fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: DWORD) -> BOOL;
            fn SetConsoleCtrlHandler(
                HandlerRoutine: Option<unsafe extern "system" fn(DWORD) -> BOOL>,
                Add: BOOL,
            ) -> BOOL;
        }

        // Safety Invariant:
        // C-FFI callback invoked by Windows on a dedicated OS thread upon receiving console events.
        // Returning 1 marks the event handled, preventing process termination.
        unsafe extern "system" fn raw_ctrl_handler(ctrl_type: DWORD) -> BOOL {
            const CTRL_C_EVENT: DWORD = 0;
            const CTRL_BREAK_EVENT: DWORD = 1;
            if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
                WAS_CANCELLED.store(true, Ordering::SeqCst);
                if let Some(tx) = NOTIFY_TX.get() {
                    let _ = tx.send(());
                }
                1
            } else {
                0
            }
        }

        /// Registers OS-level console control handler and ensures processed input is enabled.
        pub fn init_win_handler() {
            // Safety Invariant: Calls standard Win32 API with valid function pointer.
            unsafe {
                SetConsoleCtrlHandler(Some(raw_ctrl_handler), 1);
                ensure_input_mode();
            }
        }

        /// Configures standard input to ensure CTRL_C_EVENT is dispatched to handlers.
        pub fn ensure_input_mode() {
            // Safety Invariant: Calls Win32 GetStdHandle and SetConsoleMode with valid stack pointer.
            unsafe {
                let h_in = GetStdHandle(STD_INPUT_HANDLE);
                if !h_in.is_null() && h_in != (-1isize as *mut std::ffi::c_void) {
                    let mut mode: DWORD = 0;
                    if GetConsoleMode(h_in, &mut mode) != 0 {
                        SetConsoleMode(h_in, mode | ENABLE_PROCESSED_INPUT);
                    }
                }
            }
        }
    }

    /// Initializes signal handling and registers OS-level console control handlers.
    pub fn init_signals() {
        let (tx, _) = broadcast::channel(16);
        let _ = NOTIFY_TX.set(tx.clone());

        #[cfg(windows)]
        {
            win::init_win_handler();
        }

        #[cfg(not(windows))]
        {
            tokio::spawn(async move {
                while let Ok(()) = tokio::signal::ctrl_c().await {
                    WAS_CANCELLED.store(true, Ordering::SeqCst);
                    let _ = tx.send(());
                }
            });
        }
    }

    /// Ensures that console input processing mode is enabled so Ctrl+C events are dispatched.
    pub fn ensure_console_mode() {
        #[cfg(windows)]
        {
            win::ensure_input_mode();
        }
    }

    /// Subscribes to cancellation signal events.
    pub fn subscribe() -> broadcast::Receiver<()> {
        if let Some(tx) = NOTIFY_TX.get() {
            tx.subscribe()
        } else {
            init_signals();
            if let Some(tx) = NOTIFY_TX.get() {
                tx.subscribe()
            } else {
                let (_, rx) = broadcast::channel(1);
                rx
            }
        }
    }

    /// Returns whether a cancellation signal was received.
    pub fn was_cancelled() -> bool {
        WAS_CANCELLED.load(Ordering::SeqCst)
    }

    /// Resets the cancellation flag.
    pub fn reset() {
        WAS_CANCELLED.store(false, Ordering::SeqCst);
    }
}

/// Initializes console subsystem and enables Virtual Terminal Processing if supported.
pub fn init_terminal() {
    signal::init_signals();

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
            fn SetConsoleOutputCP(wCodePageID: u32) -> BOOL;
            fn SetConsoleCP(wCodePageID: u32) -> BOOL;
        }

        // SAFETY: C-FFI call to configure Windows console for UTF-8 (65001) input and output.
        unsafe {
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
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
#[cfg(windows)]
mod win_console {
    pub type HANDLE = *mut std::ffi::c_void;
    pub type BOOL = i32;
    pub type SHORT = i16;
    pub const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;

    #[repr(C)]
    pub struct COORD {
        pub x: SHORT,
        pub y: SHORT,
    }

    #[repr(C)]
    pub struct SMALL_RECT {
        pub left: SHORT,
        pub top: SHORT,
        pub right: SHORT,
        pub bottom: SHORT,
    }

    #[repr(C)]
    pub struct CONSOLE_SCREEN_BUFFER_INFO {
        pub dw_size: COORD,
        pub dw_cursor_position: COORD,
        pub w_attributes: u16,
        pub sr_window: SMALL_RECT,
        pub dw_maximum_window_size: COORD,
    }

    extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> HANDLE;
        fn GetConsoleScreenBufferInfo(
            hConsoleOutput: HANDLE,
            lpConsoleScreenBufferInfo: *mut CONSOLE_SCREEN_BUFFER_INFO,
        ) -> BOOL;
    }

    /// Query the console screen buffer info safely.
    pub fn get_buffer_info() -> Option<CONSOLE_SCREEN_BUFFER_INFO> {
        // SAFETY: GetStdHandle is a standard Win32 C-FFI call called with constant STD_OUTPUT_HANDLE.
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle.is_null() || handle == (-1isize as HANDLE) {
            return None;
        }
        let mut info = std::mem::MaybeUninit::<CONSOLE_SCREEN_BUFFER_INFO>::uninit();
        // SAFETY: GetConsoleScreenBufferInfo receives a valid handle and a pointer to uninitialized memory
        // allocated on the stack to be filled by the Windows API.
        let ok = unsafe { GetConsoleScreenBufferInfo(handle, info.as_mut_ptr()) };
        if ok != 0 {
            // SAFETY: The API returned non-zero (success), so the struct is fully initialized.
            Some(unsafe { info.assume_init() })
        } else {
            None
        }
    }
}

pub fn terminal_width() -> usize {
    #[cfg(windows)]
    {
        if let Some(info) = win_console::get_buffer_info() {
            let width = (info.sr_window.right - info.sr_window.left + 1) as usize;
            if width >= 40 {
                return width.clamp(40, 200);
            }
        }
    }

    if let Ok(cols) = std::env::var("COLUMNS") {
        if let Ok(w) = cols.parse::<usize>() {
            if w >= 40 {
                return w.clamp(40, 200);
            }
        }
    }

    80
}

/// Returns the active terminal height, bounded between 10 and 100 rows.
pub fn terminal_height() -> usize {
    #[cfg(windows)]
    {
        if let Some(info) = win_console::get_buffer_info() {
            let height = (info.sr_window.bottom - info.sr_window.top + 1) as usize;
            if height >= 10 {
                return height.clamp(10, 100);
            }
        }
    }

    if let Ok(lines) = std::env::var("LINES") {
        if let Ok(h) = lines.parse::<usize>() {
            if h >= 10 {
                return h.clamp(10, 100);
            }
        }
    }

    24
}

/// Advances vertical space so the prompt area is placed at the bottom of the terminal window.
pub fn pad_to_bottom(reserved_rows: usize) {
    #[cfg(windows)]
    {
        if let Some(info) = win_console::get_buffer_info() {
            let window_height = (info.sr_window.bottom - info.sr_window.top + 1) as usize;
            let cursor_row_in_window =
                (info.dw_cursor_position.y - info.sr_window.top).max(0) as usize;
            let target_row = window_height.saturating_sub(reserved_rows);
            if cursor_row_in_window < target_row {
                let needed = target_row - cursor_row_in_window;
                for _ in 0..needed {
                    println!();
                }
            }
            return;
        }
    }

    // Fallback for non-Windows environments
    let height = terminal_height();
    let target = height.saturating_sub(reserved_rows);
    if target > 15 {
        print!("\x1b[{}B", target);
        let _ = io::stdout().flush();
    }
}

pub fn horizontal_separator() -> String {
    let width = terminal_width().saturating_sub(2).clamp(40, 160);
    let d = dim();
    let r = reset();
    format!("{d}{}{r}", "─".repeat(width))
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

/// The 15-line Krill ASCII emblem.
pub const KRILL_ASCII: [&str; 15] = [
    "      ▄           ▄▄▄▄▄▄",
    "    ▄▄██████████▀▀▀▀▀▀▀▀",
    "     ▀▀▀▀▀▀▀▀█▄█████████",
    "        ▄██▀██▀███████▀█",
    "    ▄███▄█████████████▀▀",
    "    ███████████████▀▀",
    "   ▄████████▄▀▀▀▀",
    "  ▄█▄███████▄█▄▄",
    "  █████▄█▀▀██▄▀██▄",
    "  ▀███████▄█▄▀█▄▀▀",
    "   ▄███▀██    ▀█",
    "    ██████▄",
    "      ▀████▄▄▄      ▄▄▄▄",
    "           ▀███▀▄▄▄▄▄▄██",
    "               ▀▀▀▀▀▀▀▀▀",
];

/// Formats a path relative to the user's home directory with `~/` prefix.
pub fn format_home_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    // Strip Windows verbatim UNC prefix `\\?\` if present
    let clean = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    let clean_slash = clean.replace('\\', "/");

    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home_clean = home.replace('\\', "/");
        if clean_slash.starts_with(&home_clean) {
            let rel = &clean_slash[home_clean.len()..];
            let rel_trimmed = rel.trim_start_matches('/');
            return format!("~/{rel_trimmed}");
        }
    }
    clean_slash
}

/// Prints the modern borderless KAI welcome banner with Krill ASCII art and adjacent metadata.
pub fn print_banner(
    version: &str,
    model: &str,
    _base_url: &str,
    working_dir: &Path,
    _session_id: &str,
    tool_names: &[String],
    skills_count: usize,
) {
    let width = terminal_width();
    let c = cyan();
    let b = bold();
    let d = dim();
    let r = reset();
    let g = green();
    let rd = red();

    let user_str = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "kai-user".to_string());

    let is_unconfigured = model == crate::config::UNCONFIGURED_MODEL || model.is_empty();
    let model_line = if is_unconfigured {
        format!("{b}{rd}no model configured{r} {d}(run /model){r}")
    } else {
        format!("{g}{model}{r} {d}(Ready){r}")
    };

    let meta_lines = [
        format!("{b}{c}KAI CLI {version}{r}"),
        format!("{d}{user_str} (Autonomous Agent){r}"),
        model_line,
        format!("{d}{}{r}", format_home_path(working_dir)),
        format!(
            "{d}{} tools · {} skills · type ? for shortcuts{r}",
            tool_names.len(),
            skills_count
        ),
    ];

    println!();
    if width >= 65 {
        for (i, art_line) in KRILL_ASCII.iter().enumerate() {
            let meta = if i < meta_lines.len() {
                &meta_lines[i]
            } else {
                ""
            };
            if meta.is_empty() {
                println!("  {c}{art_line}{r}");
            } else {
                println!("  {c}{art_line:<26}{r}    {meta}");
            }
        }
    } else {
        for art_line in KRILL_ASCII.iter() {
            println!("  {c}{art_line}{r}");
        }
        println!();
        for meta in &meta_lines {
            println!("  {meta}");
        }
    }
    println!();
}

/// Prints the status footer line below the prompt divider matching the reference layout:
/// `? for shortcuts                                  accept-edits · Gemini 1.5 Flash · high`
pub fn print_prompt_footer(model: &str, git_branch: Option<&str>, auto_approve: bool) {
    let d = dim();
    let r = reset();
    let g = green();
    let c = cyan();

    let left = format!("{d}? for shortcuts{r}");
    let left_len = 15; // "? for shortcuts"

    let mode_str = if auto_approve {
        "accept-edits"
    } else {
        "confirm-edits"
    };
    let model_str = if model.is_empty() || model == crate::config::UNCONFIGURED_MODEL {
        "no-model"
    } else {
        model
    };
    let branch_str = git_branch.unwrap_or("main");

    let right = format!("{g}{mode_str}{r} {d}·{r} {c}{model_str}{r} {d}·{r} {g}{branch_str}{r}");
    let right_len = mode_str.len() + 3 + model_str.len() + 3 + branch_str.len();

    let width = terminal_width().saturating_sub(2).clamp(50, 160);
    if width > left_len + right_len + 4 {
        let spaces = width.saturating_sub(left_len + right_len);
        println!("{left}{}{right}", " ".repeat(spaces));
    } else {
        println!("{left}  {right}");
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
        "  {model_display} {d}·{r} {y}{token_str} tokens{r} {d}·{r} {g}⎇ {git_display}{r} {d}·{r} {m}{session_branch}{r} {d}·{r} {c}[Ready]{r}"
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

/// Lightweight RAII spinner providing live animated feedback while waiting for inference.
pub struct Spinner {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Spinner {
    /// Starts an asynchronous spinner animation on the current line if stdout is a terminal.
    pub fn start(message: impl Into<String>) -> Self {
        if !is_color_enabled() || !io::stdout().is_terminal() {
            return Self { stop_tx: None };
        }

        let msg = message.into();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            const FRAMES: &[&str] = &["-", "\\", "|", "/"];
            let mut i = 0;
            let start = std::time::Instant::now();
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));

            let c = cyan();
            let d = dim();
            let r = reset();

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        print!("\r                                                                                \r");
                        let _ = io::stdout().flush();
                        break;
                    }
                    _ = interval.tick() => {
                        let elapsed = start.elapsed().as_secs();
                        let frame = FRAMES[i % FRAMES.len()];
                        i += 1;
                        print!("\r  {c}{frame}{r} {d}{msg}... ({elapsed}s){r}   ");
                        let _ = io::stdout().flush();
                    }
                }
            }
        });

        Self {
            stop_tx: Some(stop_tx),
        }
    }

    /// Stops the spinner and clears the terminal line.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
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

    #[tokio::test]
    async fn test_spinner_lifecycle() {
        let mut spinner = Spinner::start("Test loading");
        spinner.stop();
    }
}
