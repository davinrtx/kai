//! Interactive terminal readline completion and hint provider for KAI CLI.
//!
//! Provides [`KaiHelper`] for real-time tab completion of slash commands (`/model`,
//! `/reasoning`, `/sessions`, `/branch`, `/compress`, `/tools`), argument completion,
//! and `@` file path context injection.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

/// Slash command registry for interactive auto-completion and help.
pub const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "View or switch active model and endpoint"),
    ("/reasoning", "Toggle or set reasoning (<think>) visibility"),
    (
        "/sessions",
        "List persistent sessions saved in .kai/sessions",
    ),
    ("/resume", "Resume past session ID and restore history"),
    ("/branch", "Fork or switch active session DAG branch"),
    ("/branches", "List all branches in the session graph"),
    ("/compress", "Condense past turns in session graph"),
    ("/compact", "Alias for /compress"),
    ("/status", "View detailed runtime telemetry"),
    ("/tools", "Display all registered agent tools"),
    ("/clear", "Clear conversation history in active session"),
    ("/help", "View help catalog of available commands"),
    ("/exit", "Quit the interactive session"),
];

/// Interactive readline helper providing completion, inline hints, and validation.
#[derive(Clone, Debug)]
pub struct KaiHelper {
    commands: Vec<(&'static str, &'static str)>,
    working_dir: PathBuf,
    session_dir: PathBuf,
    discovered_models: Arc<RwLock<Vec<String>>>,
}

impl KaiHelper {
    /// Constructs a new [`KaiHelper`] targeting the given working and session directories.
    pub fn new(working_dir: &Path, session_dir: &Path) -> Self {
        Self {
            commands: BUILTIN_SLASH_COMMANDS.to_vec(),
            working_dir: working_dir.to_path_buf(),
            session_dir: session_dir.to_path_buf(),
            discovered_models: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Constructs a [`KaiHelper`] sharing a handle to discovered model candidates.
    pub fn with_models_handle(
        working_dir: &Path,
        session_dir: &Path,
        discovered_models: Arc<RwLock<Vec<String>>>,
    ) -> Self {
        Self {
            commands: BUILTIN_SLASH_COMMANDS.to_vec(),
            working_dir: working_dir.to_path_buf(),
            session_dir: session_dir.to_path_buf(),
            discovered_models,
        }
    }

    /// Returns a clone of the shared handle to discovered model candidates.
    pub fn discovered_models_handle(&self) -> Arc<RwLock<Vec<String>>> {
        Arc::clone(&self.discovered_models)
    }

    /// Updates the cached list of discovered model candidates for autocompletion.
    pub fn set_discovered_models(&self, models: Vec<String>) {
        if let Ok(mut guard) = self.discovered_models.write() {
            *guard = models;
        }
    }

    /// Returns the registered slash commands.
    pub fn commands(&self) -> &[(&'static str, &'static str)] {
        &self.commands
    }
}

impl Completer for KaiHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let sub = &line[..pos];

        // 1. Slash command autocompletion
        if let Some(slash_idx) = sub.rfind('/') {
            let is_start = slash_idx == 0
                || sub[..slash_idx]
                    .chars()
                    .last()
                    .map(|c| c.is_whitespace())
                    .unwrap_or(false);

            if is_start {
                let typed = &sub[slash_idx..];
                let parts: Vec<&str> = sub.split_whitespace().collect();

                // Argument completion
                if parts.len() > 1 || sub.ends_with(' ') {
                    let cmd = parts[0];
                    let arg_start = sub.rfind(' ').map(|i| i + 1).unwrap_or(pos);
                    let arg_typed = &sub[arg_start..];

                    match cmd {
                        "/model" => {
                            let mut matches = Vec::new();
                            if "probe".starts_with(arg_typed) {
                                matches.push(Pair {
                                    display: "probe        Probe endpoint for models".to_string(),
                                    replacement: "probe".to_string(),
                                });
                            }
                            if let Ok(guard) = self.discovered_models.read() {
                                for m in guard.iter() {
                                    if m.starts_with(arg_typed) {
                                        matches.push(Pair {
                                            display: m.clone(),
                                            replacement: m.clone(),
                                        });
                                    }
                                }
                            }
                            matches.sort_by(|a, b| a.display.cmp(&b.display));
                            return Ok((arg_start, matches));
                        }
                        "/reasoning" => {
                            let mut matches = Vec::new();
                            for opt in &["on", "off"] {
                                if opt.starts_with(arg_typed) {
                                    matches.push(Pair {
                                        display: opt.to_string(),
                                        replacement: opt.to_string(),
                                    });
                                }
                            }
                            return Ok((arg_start, matches));
                        }
                        "/resume" => {
                            let mut matches = Vec::new();
                            if let Ok(entries) = std::fs::read_dir(&self.session_dir) {
                                for entry in entries.flatten() {
                                    let path = entry.path();
                                    if path.extension().and_then(|s| s.to_str()) == Some("json") {
                                        if let Some(stem) =
                                            path.file_stem().and_then(|s| s.to_str())
                                        {
                                            if stem.starts_with(arg_typed) {
                                                matches.push(Pair {
                                                    display: stem.to_string(),
                                                    replacement: stem.to_string(),
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            matches.sort_by(|a, b| a.display.cmp(&b.display));
                            return Ok((arg_start, matches));
                        }
                        _ => return Ok((pos, Vec::new())),
                    }
                }

                // Base slash command completion without trailing space
                let mut matches = Vec::new();
                for (cmd, desc) in &self.commands {
                    if cmd.starts_with(typed) {
                        matches.push(Pair {
                            display: format!("{:<14} {}", cmd, desc),
                            replacement: cmd.to_string(),
                        });
                    }
                }
                return Ok((slash_idx, matches));
            }
        }

        // 2. '@' File and Directory Context Path Autocompletion
        if let Some(at_idx) = sub.rfind('@') {
            let path_part = &sub[at_idx + 1..];
            let clean_part = path_part.strip_prefix("file:").unwrap_or(path_part);

            let (dir_prefix, file_prefix) = match clean_part.rfind('/') {
                Some(idx) => (&clean_part[..=idx], &clean_part[idx + 1..]),
                None => ("", clean_part),
            };

            let search_dir = self.working_dir.join(dir_prefix);
            let mut matches = Vec::new();

            if let Ok(entries) = std::fs::read_dir(&search_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with('.') && !file_prefix.starts_with('.') {
                        continue;
                    }
                    if name.starts_with(file_prefix) {
                        let is_dir = entry.path().is_dir();
                        let suffix = if is_dir { "/" } else { "" };
                        let full_token = format!("@{dir_prefix}{name}{suffix}");
                        let display_label = format!("{}{}", name, if is_dir { "/" } else { "" });
                        matches.push(Pair {
                            display: display_label,
                            replacement: full_token,
                        });
                    }
                }
            }
            matches.sort_by(|a, b| a.display.cmp(&b.display));
            return Ok((at_idx, matches));
        }

        Ok((pos, Vec::new()))
    }
}

impl Hinter for KaiHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<String> {
        if pos < line.len() {
            return None;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            for (cmd, _) in &self.commands {
                if cmd.starts_with(trimmed) && *cmd != trimmed {
                    return Some(cmd[trimmed.len()..].to_string());
                }
            }
        }
        None
    }
}

impl Highlighter for KaiHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> {
        std::borrow::Cow::Owned(format!("\x1b[2m\x1b[90m{hint}\x1b[0m"))
    }
}
impl Validator for KaiHelper {}
impl Helper for KaiHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_completer_slash_prefix() {
        let temp_dir = std::env::temp_dir();
        let helper = KaiHelper::new(&temp_dir, &temp_dir);

        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);

        let (idx, candidates) = helper.complete("/m", 2, &ctx).unwrap();
        assert_eq!(idx, 0);
        assert!(!candidates.is_empty());
        assert_eq!(candidates[0].replacement, "/model");
    }

    #[test]
    fn test_completer_reasoning_args() {
        let temp_dir = std::env::temp_dir();
        let helper = KaiHelper::new(&temp_dir, &temp_dir);

        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);

        let (idx, candidates) = helper.complete("/reasoning o", 12, &ctx).unwrap();
        assert_eq!(idx, 11);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].replacement, "on");
        assert_eq!(candidates[1].replacement, "off");
    }

    #[test]
    fn test_completer_model_args() {
        let temp_dir = std::env::temp_dir();
        let helper = KaiHelper::new(&temp_dir, &temp_dir);
        helper.set_discovered_models(vec![
            "deepseek-coder:6.7b".to_string(),
            "qwen2.5-coder:7b".to_string(),
        ]);

        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);

        // Complete prefix "q"
        let (idx, candidates) = helper.complete("/model q", 8, &ctx).unwrap();
        assert_eq!(idx, 7);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].replacement, "qwen2.5-coder:7b");

        // Complete empty prefix shows probe and models
        let (idx2, candidates2) = helper.complete("/model ", 7, &ctx).unwrap();
        assert_eq!(idx2, 7);
        assert_eq!(candidates2.len(), 3); // probe, deepseek-coder:6.7b, qwen2.5-coder:7b
    }

    #[test]
    fn test_hinter_slash_command() {
        let temp_dir = std::env::temp_dir();
        let helper = KaiHelper::new(&temp_dir, &temp_dir);

        let history = rustyline::history::DefaultHistory::new();
        let ctx = Context::new(&history);

        let hint = helper.hint("/mo", 3, &ctx);
        assert_eq!(hint.as_deref(), Some("del"));
    }
}
