//! Configuration resolution for the KAI CLI.
//!
//! Handles environment variables, CLI overrides, and sensible defaults
//! for model inference endpoints, working directories, and execution budgets.

use std::env;
use std::path::PathBuf;

/// Default local OpenAI-compatible inference endpoint (Ollama standard).
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

/// Marker string designating that no inference model has been configured.
pub const UNCONFIGURED_MODEL: &str = "unconfigured";

/// Default code generation model fallback when requested.
pub const DEFAULT_MODEL: &str = "qwen2.5-coder:7b";

/// Default maximum consecutive reasoning turns.
pub const DEFAULT_MAX_TURNS: usize = 50;

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
    /// Resolves configuration combining defaults, environment variables, and CLI overrides.
    pub fn resolve(
        base_url_override: Option<String>,
        model_override: Option<String>,
        api_key_override: Option<String>,
        working_dir_override: Option<PathBuf>,
        max_turns_override: Option<usize>,
        auto_approve: bool,
    ) -> Result<Self, crate::error::CliError> {
        let base_url = base_url_override
            .or_else(|| env::var("KAI_BASE_URL").ok())
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

        let model = model_override
            .or_else(|| env::var("KAI_MODEL").ok())
            .or_else(|| env::var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| UNCONFIGURED_MODEL.to_string());

        let api_key = api_key_override
            .or_else(|| env::var("KAI_API_KEY").ok())
            .or_else(|| env::var("OPENAI_API_KEY").ok());

        let working_dir = match working_dir_override {
            Some(dir) => dir,
            None => env::current_dir().map_err(crate::error::CliError::Io)?,
        };

        let max_turns = max_turns_override.unwrap_or(DEFAULT_MAX_TURNS);

        let system_prompt = format!(
            "You are KAI (Krill Agent Interface), an autonomous software engineering agent.\n\
            You inspect, modify, and verify codebases using bounded, deterministic tools.\n\
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
