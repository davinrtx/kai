//! Kernel-level container and sandbox process isolation engine.
//!
//! Provides [`BubblewrapIsolationEngine`] implementing [`kai_core::traits::CommandIsolationEngine`].
//! Enforces Linux Bubblewrap (`bwrap`) containment (unshared network namespaces, read-only system roots,
//! masked `~/.ssh` and host home credentials) with deterministic fallback for non-bwrap environments.

use std::path::{Path, PathBuf};

use kai_core::error::{KaiError, Result};
use kai_core::traits::{CommandIsolationEngine, IsolatedCommandSpec};

use crate::command::{CommandSanitizer, BLOCKED_ENV_SUBSTRINGS};

/// Container isolation engine leveraging Linux Bubblewrap (`bwrap`) with unprivileged sandboxing.
#[derive(Debug, Clone)]
pub struct BubblewrapIsolationEngine {
    bwrap_path: Option<PathBuf>,
}

impl Default for BubblewrapIsolationEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl BubblewrapIsolationEngine {
    /// Constructs a new [`BubblewrapIsolationEngine`] automatically probing for `/usr/bin/bwrap`.
    pub fn new() -> Self {
        let default_path = PathBuf::from("/usr/bin/bwrap");
        let path = if default_path.exists() {
            Some(default_path)
        } else {
            None
        };
        Self { bwrap_path: path }
    }

    /// Constructs an engine with an explicitly configured bubblewrap executable path.
    pub fn with_bwrap_path(bwrap_path: Option<PathBuf>) -> Self {
        Self { bwrap_path }
    }

    /// Returns whether Bubblewrap container isolation is supported on this host.
    pub fn is_bwrap_supported(&self) -> bool {
        self.bwrap_path.is_some()
    }

    /// Sanitizes host environment variables, removing sensitive tokens, keys, and credentials.
    pub fn scrubbed_environment(&self) -> Vec<(String, String)> {
        let mut clean_env = Vec::new();
        for (k, v) in std::env::vars() {
            let upper = k.to_uppercase();
            let is_blocked = BLOCKED_ENV_SUBSTRINGS.iter().any(|b| upper.contains(b))
                || upper.starts_with("SSH_")
                || upper.starts_with("AWS_")
                || upper.starts_with("GITHUB_")
                || upper.starts_with("KAI_API_");

            if !is_blocked {
                clean_env.push((k, v));
            }
        }
        clean_env
    }
}

impl CommandIsolationEngine for BubblewrapIsolationEngine {
    fn wrap_command(
        &self,
        command: &str,
        working_dir: &Path,
        allow_network: bool,
    ) -> Result<IsolatedCommandSpec> {
        // Enforce basic command safety checks first
        let sanitizer = CommandSanitizer::new();
        sanitizer
            .validate_command(command)
            .map_err(KaiError::Sandbox)?;

        let clean_env = self.scrubbed_environment();

        if let Some(ref bwrap) = self.bwrap_path {
            // Linux Bubblewrap container isolation
            let mut args = Vec::new();
            args.push("--unshare-all".to_string());

            if !allow_network {
                args.push("--unshare-net".to_string());
            } else {
                args.push("--share-net".to_string());
            }

            // Mount core system paths read-only
            if Path::new("/usr").exists() {
                args.push("--ro-bind".to_string());
                args.push("/usr".to_string());
                args.push("/usr".to_string());
            }
            if Path::new("/lib").exists() {
                args.push("--ro-bind".to_string());
                args.push("/lib".to_string());
                args.push("/lib".to_string());
            }
            if Path::new("/lib64").exists() {
                args.push("--ro-bind".to_string());
                args.push("/lib64".to_string());
                args.push("/lib64".to_string());
            }
            if Path::new("/bin").exists() {
                args.push("--ro-bind".to_string());
                args.push("/bin".to_string());
                args.push("/bin".to_string());
            }
            if Path::new("/etc").exists() {
                args.push("--ro-bind".to_string());
                args.push("/etc".to_string());
                args.push("/etc".to_string());
            }

            // Virtual filesystems
            args.push("--proc".to_string());
            args.push("/proc".to_string());
            args.push("--dev".to_string());
            args.push("/dev".to_string());
            args.push("--tmpfs".to_string());
            args.push("/tmp".to_string());

            // Mask home directory to block access to ~/.ssh, ~/.aws, credentials
            args.push("--tmpfs".to_string());
            args.push("/home".to_string());

            // Bind isolated working directory read-write
            let dir_str = working_dir.to_string_lossy().to_string();
            args.push("--bind".to_string());
            args.push(dir_str.clone());
            args.push(dir_str);

            args.push("--chdir".to_string());
            args.push(working_dir.to_string_lossy().to_string());

            // Clear environment and pass shell invocation
            args.push("--clearenv".to_string());
            args.push("--setenv".to_string());
            args.push("PATH".to_string());
            args.push("/usr/local/bin:/usr/bin:/bin".to_string());
            args.push("--setenv".to_string());
            args.push("HOME".to_string());
            args.push("/tmp".to_string());

            args.push("/bin/sh".to_string());
            args.push("-c".to_string());
            args.push(command.to_string());

            Ok(IsolatedCommandSpec {
                program: bwrap.clone(),
                args,
                env: clean_env,
            })
        } else {
            // Portable host shell fallback (Windows / Unix non-bwrap hosts)
            #[cfg(windows)]
            let (program, args) = (
                PathBuf::from("cmd.exe"),
                vec!["/C".to_string(), command.to_string()],
            );

            #[cfg(not(windows))]
            let (program, args) = (
                PathBuf::from("/bin/sh"),
                vec!["-c".to_string(), command.to_string()],
            );

            Ok(IsolatedCommandSpec {
                program,
                args,
                env: clean_env,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bwrap_engine_command_generation_and_env_scrub() {
        let engine =
            BubblewrapIsolationEngine::with_bwrap_path(Some(PathBuf::from("/usr/bin/bwrap")));
        assert!(engine.is_bwrap_supported());

        let dir = Path::new("/workspace");
        let spec = engine
            .wrap_command("ls -la", dir, false)
            .expect("wrap command");

        assert_eq!(spec.program, PathBuf::from("/usr/bin/bwrap"));
        assert!(spec.args.contains(&"--unshare-all".to_string()));
        assert!(spec.args.contains(&"--unshare-net".to_string()));
        assert!(spec.args.contains(&"/workspace".to_string()));
        assert!(spec.args.contains(&"ls -la".to_string()));

        // Check network allowed
        let net_spec = engine
            .wrap_command("curl https://example.com", dir, true)
            .expect("wrap net command");
        assert!(net_spec.args.contains(&"--share-net".to_string()));
        assert!(!net_spec.args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn test_portable_fallback_when_bwrap_absent() {
        let engine = BubblewrapIsolationEngine::with_bwrap_path(None);
        assert!(!engine.is_bwrap_supported());

        let dir = Path::new("/workspace");
        let spec = engine
            .wrap_command("echo hello", dir, false)
            .expect("fallback spec");

        #[cfg(windows)]
        assert_eq!(spec.program, PathBuf::from("cmd.exe"));
        #[cfg(not(windows))]
        assert_eq!(spec.program, PathBuf::from("/bin/sh"));

        assert!(spec.args.iter().any(|a| a.contains("echo hello")));
    }

    #[test]
    fn test_blocks_destructive_commands() {
        let engine = BubblewrapIsolationEngine::new();
        let dir = Path::new("/workspace");
        let res = engine.wrap_command("rm -rf /", dir, false);
        assert!(res.is_err());
    }
}
