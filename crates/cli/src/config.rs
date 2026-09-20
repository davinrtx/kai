//! Configuration resolution for the KAI CLI.
//!
//! Handles environment variables, CLI overrides, and sensible defaults
//! for model inference endpoints, working directories, and execution budgets.

use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default local OpenAI-compatible inference endpoint (Ollama standard).
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Marker string designating that no inference model has been configured.
pub const UNCONFIGURED_MODEL: &str = "unconfigured";

/// Default code generation model fallback when requested.
pub const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";

/// Default maximum consecutive reasoning turns.
pub const DEFAULT_MAX_TURNS: usize = 50;

/// Persistent configuration model saved to `.kai/config.json` or `~/.kai/config.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KaiConfigFile {
    /// OpenAI-compatible inference endpoint base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional authorization API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Maximum consecutive reasoning turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    /// Auto-approve tool execution flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<bool>,
}

impl KaiConfigFile {
    /// Finds the existing `.kai/config.json` either in local working dir or global home dir.
    pub fn locate(working_dir: &Path) -> Option<PathBuf> {
        let local = working_dir.join(".kai").join("config.json");
        if local.exists() {
            return Some(local);
        }

        if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            let global = PathBuf::from(home).join(".kai").join("config.json");
            if global.exists() {
                return Some(global);
            }
        }

        None
    }

    /// Loads the configuration from the active path.
    pub fn load(working_dir: &Path) -> Option<Self> {
        let path = Self::locate(working_dir)?;
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    /// Atomically and transactionally writes configuration to `.kai/config.json`.
    pub fn save(&self, working_dir: &Path) -> Result<PathBuf, crate::error::CliError> {
        let kai_dir = working_dir.join(".kai");
        std::fs::create_dir_all(&kai_dir).map_err(crate::error::CliError::Io)?;

        let target = kai_dir.join("config.json");
        let tmp = kai_dir.join("config.json.tmp");

        let serialized =
            serde_json::to_string_pretty(self).map_err(crate::error::CliError::Json)?;
        std::fs::write(&tmp, serialized).map_err(crate::error::CliError::Io)?;

        #[cfg(windows)]
        let _ = std::fs::remove_file(&target);

        std::fs::rename(&tmp, &target).map_err(crate::error::CliError::Io)?;
        Ok(target)
    }
}

/// Runtime configuration for a KAI CLI execution session.
#[derive(Debug, Clone)]
pub struct KaiConfig {
    /// OpenAI-compatible inference endpoint base URL (e.g. `http://localhost:11434/v1`).
    pub base_url: String,
    /// Model identifier passed in chat completions request.
    pub model: String,
    /// Optional API key for authenticated inference endpoints.
    pub api_key: Option<String>,
    /// Working directory for tool operations and sandbox confinement.
    pub working_dir: PathBuf,
    /// Maximum consecutive reasoning turns before terminating.
    pub max_turns: usize,
    /// Whether to auto-approve tool execution without interactive confirmation.
    pub auto_approve: bool,
    /// Base system prompt defining the agent's identity and operational constraints.
    pub system_prompt: String,
}

impl KaiConfig {
    /// Resolves configuration combining defaults, environment variables, config file, and CLI overrides.
    pub fn resolve(
        base_url_override: Option<String>,
        model_override: Option<String>,
        api_key_override: Option<String>,
        working_dir_override: Option<PathBuf>,
        max_turns_override: Option<usize>,
        auto_approve: bool,
    ) -> Result<Self, crate::error::CliError> {
        let working_dir = match working_dir_override {
            Some(dir) => dir,
            None => env::current_dir().map_err(crate::error::CliError::Io)?,
        };

        let file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();

        let base_url = base_url_override
            .or_else(|| env::var("KAI_BASE_URL").ok())
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .or(file_cfg.base_url)
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let model = model_override
            .or_else(|| env::var("KAI_MODEL").ok())
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .or(file_cfg.model)
            .unwrap_or_else(|| UNCONFIGURED_MODEL.to_string());

        let api_key = api_key_override
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                env::var("KAI_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                env::var("OPENROUTER_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                env::var("DEEPSEEK_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                env::var("GROQ_API_KEY")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or(file_cfg.api_key);

        let max_turns = max_turns_override
            .or(file_cfg.max_turns)
            .unwrap_or(DEFAULT_MAX_TURNS);

        let auto_approve = if auto_approve {
            true
        } else {
            file_cfg.auto_approve.unwrap_or(false)
        };

        let system_prompt = format!(
            "You are KAI (Krill Agent Interface), an autonomous software engineering agent.\n\
            You inspect, modify, and verify codebases using bounded, deterministic tools.\n\
            Use `list_dir` to inspect directory contents and `read_window` to read file contents.\n\
            Do not execute shell commands (e.g. powershell, dir, ls) for filesystem inspection when native tools are available.\n\
            Always read files using bounded windows and verify your changes with non-interactive commands.\n\
            Working directory: {}",
            working_dir.display()
        );

        Ok(Self {
            base_url,
            model,
            api_key,
            working_dir,
            max_turns,
            auto_approve,
            system_prompt,
        })
    }

    /// Canonicalizes the working directory ensuring it exists on disk.
    pub fn canonical_working_dir(&self) -> Result<PathBuf, crate::error::CliError> {
        self.working_dir
            .canonicalize()
            .map_err(crate::error::CliError::Io)
    }
}
