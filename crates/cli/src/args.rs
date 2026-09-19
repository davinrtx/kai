//! Command-line argument schemas for KAI CLI.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// KAI (Krill Agent Interface) - Autonomous software engineering agent runtime.
#[derive(Debug, Parser)]
#[command(name = "kai", version, about = "Autonomous software engineering agent CLI", long_about = None)]
pub struct CliArgs {
    /// Subcommand to execute. If omitted, positional prompt or flags are evaluated.
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Shorthand prompt for one-shot execution without typing `run`.
    #[arg(short = 'p', long = "prompt", global = true)]
    pub prompt: Option<String>,

    /// OpenAI-compatible model name override (e.g. `qwen2.5-coder:7b`, `gpt-4o`).
    #[arg(short = 'm', long = "model", global = true)]
    pub model: Option<String>,

    /// OpenAI-compatible inference endpoint URL override (e.g. `http://localhost:11434/v1`).
    #[arg(short = 'u', long = "base-url", global = true)]
    pub base_url: Option<String>,

    /// API key for authenticated inference endpoints.
    #[arg(short = 'k', long = "api-key", global = true)]
    pub api_key: Option<String>,

    /// Automatically approve all tool execution confirmations non-interactively.
    #[arg(short = 'y', long = "yes", global = true)]
    pub yes: bool,

    /// Maximum consecutive reasoning turns allowed per task.
    #[arg(short = 't', long = "max-turns", global = true)]
    pub max_turns: Option<usize>,

    /// Canonical working directory for the agent execution session.
    #[arg(short = 'd', long = "dir", global = true)]
    pub working_dir: Option<PathBuf>,
}

/// Available subcommands within the KAI CLI.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Executes a single discrete engineering task non-interactively and exits.
    Run(RunCommand),

    /// Starts an interactive multi-turn conversational REPL in the terminal.
    Chat(ChatCommand),

    /// Launches a background daemon supervisor for autonomous task processing.
    Daemon(DaemonCommand),

    /// Introspects and displays the catalog of registered agent tools.
    Tools(ToolsCommand),
}

/// Arguments for the `run` subcommand.
#[derive(Debug, Args)]
pub struct RunCommand {
    /// Task description or prompt for the agent to execute.
    pub task: String,
}

/// Arguments for the `chat` subcommand.
#[derive(Debug, Args, Default)]
pub struct ChatCommand {}

/// Arguments for the `daemon` subcommand.
#[derive(Debug, Args, Default)]
pub struct DaemonCommand {}

/// Arguments for the `tools` subcommand.
#[derive(Debug, Args, Default)]
pub struct ToolsCommand {
    /// Output the tool schemas in raw JSON format.
    #[arg(long = "json")]
    pub json: bool,
}
