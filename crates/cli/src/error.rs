//! Strongly typed error definitions for the KAI CLI application.

use thiserror::Error;

/// Operational errors encountered within the KAI CLI runtime.
#[derive(Debug, Error)]
pub enum CliError {
    /// Filesystem or I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP network communication failure.
    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization or deserialization failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Error propagated from the underlying KAI core runtime.
    #[error("Runtime error: {0}")]
    Core(#[from] kai_core::error::KaiError),

    /// Inference API returned an error response.
    #[error("Inference API error (HTTP {status}): {message}")]
    Api {
        /// HTTP status code.
        status: u16,
        /// Detail or body returned from endpoint.
        message: String,
    },

    /// Invalid configuration setting or missing parameter.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Session was interrupted or aborted by user or steering signal.
    #[error("Execution interrupted: {0}")]
    Interrupted(String),
}

/// Specialized [`Result`](std::result::Result) type alias for CLI operations.
pub type Result<T> = std::result::Result<T, CliError>;
