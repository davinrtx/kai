//! Shell command validation, destructive pattern blocking, and environment variable scrubbing.
//!
//! Provides [`CommandSanitizer`] to safeguard subprocess execution, block interactive
//! STDIN deadlocks, detect destructive system commands, and filter sensitive credentials
//! from subprocess environments.

use kai_core::error::SandboxError;

/// Blocklist of sensitive environment variable identifiers and sub-patterns.
pub const BLOCKED_ENV_SUBSTRINGS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "TOKEN",
    "PRIVATE_KEY",
    "AUTH_TOKEN",
    "ACCESS_TOKEN",
    "DATABASE_URL",
];

/// Known destructive command signatures and dangerous shell patterns.
const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "rm -rf /*",
    "rm -fr /*",
    "rm -rf ~",
    "rm -fr ~",
    ":(){ :|:& };:",
    ":(){:|:&};:",
    "mkfs",
    "fdisk",
    "parted",
    "shutdown -h",
    "shutdown -r",
    "shutdown /s",
    "shutdown /r",
    "init 0",
    "init 6",
    "halt -f",
    "reboot -f",
    // Windows destructive commands
    "del /s",
    "del /f /s",
    "del /s /f",
    "del /s /q",
    "del /q /s",
    "rd /s /q",
    "rd /q /s",
    "rmdir /s /q",
    "rmdir /q /s",
    "format c:",
    "format d:",
];

/// Helper to split a command string by pipeline and chaining operators (;, &&, ||, |)
/// while respecting single and double quoted regions.
fn split_command_chain(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut last = 0;
    let mut in_single = false;
    let mut in_double = false;
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        let b = bytes[i];
        if b == b'\'' && !in_double {
            in_single = !in_single;
        } else if b == b'"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double {
            if b == b';' || b == b'|' {
                segments.push(&command[last..i]);
                if i + 1 < len && (bytes[i + 1] == b'|' || bytes[i + 1] == b'&') {
                    i += 1;
                }
                last = i + 1;
            } else if b == b'&' {
                if i + 1 < len && bytes[i + 1] == b'&' {
                    segments.push(&command[last..i]);
                    i += 1;
                    last = i + 1;
                } else {
                    segments.push(&command[last..i]);
                    last = i + 1;
                }
            }
        }
        i += 1;
    }
    if last < len {
        segments.push(&command[last..]);
    }
    segments
}

/// Shell command and process execution security validator.
#[derive(Debug, Clone, Default)]
pub struct CommandSanitizer {
    custom_blocked_patterns: Vec<String>,
}

impl CommandSanitizer {
    /// Constructs a default [`CommandSanitizer`].
    pub fn new() -> Self {
        Self {
            custom_blocked_patterns: Vec::new(),
        }
    }

    /// Adds custom blocked command substrings or signatures.
    pub fn with_blocked_pattern(mut self, pattern: impl Into<String>) -> Self {
        self.custom_blocked_patterns.push(pattern.into());
        self
    }

    /// Validates a shell command string, ensuring it is non-destructive and non-blocking.
    ///
    /// Checks for:
    /// - Destructive system wiping commands (`rm -rf /`, Windows `del /s`, `format`, `mkfs`, fork bombs)
    /// - Raw block device writes via `dd` or redirects
    /// - Interactive prompts that block indefinitely on `STDIN` (`sudo`, `passwd`) across chained commands
    pub fn validate_command(&self, command: &str) -> Result<(), SandboxError> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        // Normalize whitespace for robust pattern and token checking
        let normalized_lower = trimmed
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();

        // 1. Check destructive command patterns on normalized string
        for pattern in DESTRUCTIVE_PATTERNS {
            if normalized_lower.contains(pattern) {
                return Err(SandboxError::PermissionDenied {
                    operation: "exec_command".to_string(),
                    resource: format!("Destructive command pattern '{pattern}' is blocked"),
                });
            }
        }

        // Check Windows disk format invocation
        if normalized_lower.starts_with("format ") || normalized_lower.contains(" format ") {
            return Err(SandboxError::PermissionDenied {
                operation: "exec_command".to_string(),
                resource: "Disk format command is blocked".to_string(),
            });
        }

        // 2. Check custom blocked patterns
        for pattern in &self.custom_blocked_patterns {
            if normalized_lower.contains(&pattern.to_ascii_lowercase()) {
                return Err(SandboxError::PermissionDenied {
                    operation: "exec_command".to_string(),
                    resource: format!("Custom blocked pattern '{pattern}' matched"),
                });
            }
        }

        // 3. Block raw writes to disk devices via dd
        if normalized_lower.contains("dd ")
            && (normalized_lower.contains("of=/dev/") || normalized_lower.contains(r"of=\\.\"))
        {
            return Err(SandboxError::PermissionDenied {
                operation: "exec_command".to_string(),
                resource: "Direct raw block device write via dd is blocked".to_string(),
            });
        }

        // 4. Validate each chained subcommand for interactive prompts and destructive operations
        let segments = split_command_chain(trimmed);
        for segment in segments {
            let seg_trimmed = segment.trim();
            if seg_trimmed.is_empty() {
                continue;
            }

            let tokens: Vec<&str> = seg_trimmed.split_whitespace().collect();
            let Some(first_token) = tokens.first() else {
                continue;
            };

            let base_cmd = match first_token.rsplit(['/', '\\']).next() {
                Some(cmd) => cmd.to_ascii_lowercase(),
                None => first_token.to_ascii_lowercase(),
            };

            // Block interactive commands
            if base_cmd == "sudo" || base_cmd == "su" || base_cmd == "passwd" {
                return Err(SandboxError::PermissionDenied {
                    operation: "exec_command".to_string(),
                    resource: format!(
                        "Interactive authentication command '{base_cmd}' is blocked in non-interactive agent runtime"
                    ),
                });
            }

            // Enforce explicit non-interactive flags on common package managers
            if (base_cmd == "apt" || base_cmd == "apt-get") && tokens.contains(&"install") {
                let has_yes = tokens
                    .iter()
                    .any(|t| *t == "-y" || *t == "--yes" || *t == "-q");
                if !has_yes {
                    return Err(SandboxError::PermissionDenied {
                        operation: "exec_command".to_string(),
                        resource: "Package installation commands must include explicit non-interactive flags (e.g. -y)".to_string(),
                    });
                }
            }

            // Detect split-flag recursive root deletions (e.g. `rm -r -f /`, `rm -f -r /*`, `rm -R ~`)
            if base_cmd == "rm" {
                let has_recursive = tokens
                    .iter()
                    .any(|t| t.starts_with('-') && (t.contains('r') || t.contains('R')));
                let targets_root = tokens
                    .iter()
                    .any(|t| *t == "/" || *t == "/*" || *t == "~" || *t == "--no-preserve-root");
                if has_recursive && targets_root {
                    return Err(SandboxError::PermissionDenied {
                        operation: "exec_command".to_string(),
                        resource: "Destructive recursive removal of root directory is blocked"
                            .to_string(),
                    });
                }
            }
        }

        Ok(())
    }

    /// Evaluates whether an environment variable name is considered sensitive and must be scrubbed.
    pub fn is_sensitive_env_var(key: &str) -> bool {
        let upper = key.to_ascii_uppercase();
        for sub in BLOCKED_ENV_SUBSTRINGS {
            if upper.contains(sub) {
                return true;
            }
        }
        false
    }

    /// Scrubs sensitive credentials, tokens, and secret variables from an environment list.
    pub fn scrub_env<K, V>(env: impl IntoIterator<Item = (K, V)>) -> Vec<(String, String)>
    where
        K: Into<String>,
        V: Into<String>,
    {
        env.into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .filter(|(k, _)| !Self::is_sensitive_env_var(k))
            .collect()
    }
}
