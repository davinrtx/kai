//! Handler for the `kai run` subcommand (one-shot non-interactive task execution).

use std::sync::Arc;

use kai_core::event::{Event, EventBus};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::ToolResultCache;
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_tools::default_tools;
use tokio::sync::Mutex;

use crate::agent::LlmAgent;
use crate::args::RunCommand;
use crate::client::ModelClient;
use crate::config::KaiConfig;
use crate::error::Result;
use crate::ui::{self, CliApprovalPolicy};

/// Executes a single discrete task via the autonomous agent engine.
pub async fn execute(cmd: RunCommand, config: KaiConfig) -> Result<()> {
    let working_dir = config.canonical_working_dir()?;
    let session_id = format!("session-run-{}", current_timestamp_ms());

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
        &session_id,
        &tool_names,
        skills_count,
    );
    ui::print_info(&format!("Task: {}", cmd.task));
    ui::print_info(&format!("Working directory: {}", working_dir.display()));

    let inbox = Arc::new(TaskInbox::new(128));
    let client = Arc::new(ModelClient::new(
        &config.base_url,
        &config.model,
        config.api_key.clone(),
    ));

    let tool_schemas: Vec<serde_json::Value> = tools.iter().map(|t| t.schema()).collect();

    let agent = Arc::new(Mutex::new(
        LlmAgent::new(
            "kai-main-agent",
            "KAI Autonomous Engineer",
            &config.system_prompt,
            client,
        )
        .with_tool_schemas(tool_schemas),
    ));

    let event_bus = EventBus::new(128);
    let mut event_rx = event_bus.subscribe();

    let mut engine = OrchestrationEngine::new(agent, inbox.clone(), &working_dir, &session_id)
        .with_max_turns(config.max_turns)
        .with_approval_policy(Arc::new(CliApprovalPolicy::new(config.auto_approve)))
        .with_tool_cache(Arc::new(ToolResultCache::default()))
        .with_event_bus(event_bus);

    for tool in tools {
        engine.register_tool(tool);
    }

    // Spawn telemetry event listener for live terminal output
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            match event {
                Event::ToolInvoked { tool_call, .. } => {
                    ui::print_tool_invocation(&tool_call.name, &tool_call.arguments);
                }
                Event::ToolCompleted { tool_result, .. } => {
                    ui::print_tool_result(
                        &tool_result.tool_call_id,
                        tool_result.is_error,
                        &tool_result.output,
                    );
                }
                Event::Interrupted { reason, .. } => {
                    ui::print_thought(&format!("Suspended/Interrupted: {reason}"));
                }
                _ => {}
            }
        }
    });

    // Enqueue initial user task into the inbox
    let user_msg = Message::user(format!("msg_user_{}", current_timestamp_ms()), cmd.task);
    inbox
        .enqueue(user_msg)
        .await
        .map_err(crate::error::CliError::Core)?;

    let outcome = engine.run().await.map_err(crate::error::CliError::Core)?;

    ui::print_assistant_response(&outcome.text_content());

    Ok(())
}
