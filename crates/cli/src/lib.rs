//! # kai-cli
//!
//! Autonomous software engineering agent CLI runtime for KAI (Krill Agent Interface).
//!
//! Exposes the core components for command-line parsing, inference client communication,
//! terminal output rendering, and execution workflows.

pub mod agent;
pub mod args;
pub mod client;
pub mod commands;
pub mod config;
pub mod error;
pub mod ui;

pub use agent::LlmAgent;
pub use args::{CliArgs, Commands, RunCommand};
pub use client::{ChatResponse, HttpTransport, LlmTransport, ModelClient};
pub use config::KaiConfig;
pub use error::{CliError, Result};
pub use ui::CliApprovalPolicy;
