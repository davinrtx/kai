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
    "rm -rf ~",
    ":(){ :|:& };:",
    ":(){:|:&};:",
    "mkfs",
    "fdisk",
    "parted",
    "shutdown -h",
    "shutdown -r",
    "init 0",
    "init 6",
    "halt -f",
    "reboot -f",
];

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
    /// - Destructive system wiping commands (`rm -rf /`, `mkfs`, fork bombs)
    /// - Raw block device writes via `dd` or redirects
    /// - Interactive prompts that block indefinitely on `STDIN` (`sudo`, `passwd`)
    pub fn validate_command(&self, command: &str) -> Result<(), SandboxError> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        let lower = trimmed.to_ascii_lowercase();

        // 1. Check destructive command patterns
        for pattern in DESTRUCTIVE_PATTERNS {
            if lower.contains(pattern) {
                return Err(SandboxError::PermissionDenied {
                    operation: "exec_command".to_string(),
                    resource: format!("Destructive command pattern '{pattern}' is blocked"),
                });
            }
        }

        // 2. Check custom blocked patterns
        for pattern in &self.custom_blocked_patterns {
            if lower.contains(&pattern.to_ascii_lowercase()) {
                return Err(SandboxError::PermissionDenied {
                    operation: "exec_command".to_string(),
                    resource: format!("Custom blocked pattern '{pattern}' matched"),
                });
            }
        }

        // 3. Block raw writes to disk devices via dd
        if lower.contains("dd ") && (lower.contains("of=/dev/") || lower.contains(r"of=\\.\")) {
            return Err(SandboxError::PermissionDenied {
                operation: "exec_command".to_string(),
                resource: "Direct raw block device write via dd is blocked".to_string(),
            });
        }

        // 4. Block commands that inherently require interactive human password input
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if let Some(first_token) = tokens.first() {
            let base_cmd = match first_token.rsplit(['/', '\\']).next() {
                Some(cmd) => cmd,
                None => first_token,
            };

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
