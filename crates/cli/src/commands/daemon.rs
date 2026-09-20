//! Handler for the `kai daemon` subcommand.

use std::sync::Arc;
use std::time::Duration;

use kai_orchestrator::daemon::DaemonSupervisor;
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_tools::default_tools;
use tokio::sync::Mutex;

use crate::agent::LlmAgent;
use crate::args::DaemonCommand;
use crate::client::ModelClient;
use crate::config::KaiConfig;
use crate::error::Result;
use crate::ui;

/// Executes the long-running daemon supervisor.
pub async fn execute(_cmd: DaemonCommand, config: KaiConfig) -> Result<()> {
    let working_dir = config.canonical_working_dir()?;
    let tools = default_tools();
    let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let skills_dir = working_dir.join(".kai").join("skills");
    let skills_count = std::fs::read_dir(&skills_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
                .count()
        })
        .unwrap_or(0);

    ui::print_banner(
        env!("CARGO_PKG_VERSION"),
        &config.model,
        &config.base_url,
        &working_dir,
        "session-daemon-01",
        &tool_names,
        skills_count,
    );
    ui::print_info(&format!(
        "Starting KAI background daemon supervisor in {}",
        working_dir.display()
    ));

    let inbox = Arc::new(TaskInbox::new(256));
    let client = Arc::new(ModelClient::new(
        &config.base_url,
        &config.model,
        config.api_key.clone(),
    ));

    let tool_schemas = crate::client::build_tool_schemas(&tools);

    let agent = Arc::new(Mutex::new(
        LlmAgent::new(
            "kai-daemon-agent",
            "KAI Daemon",
            &config.system_prompt,
            client,
        )
        .with_tool_schemas(tool_schemas),
    ));

    let mut engine = OrchestrationEngine::new(agent, inbox, &working_dir, "session-daemon-01")
        .with_compressor(Arc::new(kai_context::SemanticCommandCompressor::new()));
    for tool in tools {
        engine.register_tool(tool);
    }

    let supervisor = Arc::new(DaemonSupervisor::new(engine, Duration::from_millis(500)));
    let supervisor_clone = supervisor.clone();

    // Spawn signal handler for graceful shutdown on Ctrl+C
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            ui::print_info("Shutdown signal received (SIGINT). Stopping daemon supervisor...");
            let _ = supervisor_clone.request_shutdown().await;
        }
    });

    ui::print_info("Daemon supervisor is actively listening for incoming tasks...");
    supervisor
        .run()
        .await
        .map_err(crate::error::CliError::Core)?;
    ui::print_info("Daemon supervisor terminated cleanly.");

    Ok(())
}
