//! Handler for the `kai chat` subcommand (interactive multi-turn REPL).

use std::io::{self, Write};
use std::sync::Arc;

use kai_core::event::{Event, EventBus};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::StepOutcome;
use kai_core::ToolResultCache;
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_tools::default_tools;
use tokio::sync::Mutex;

use crate::agent::LlmAgent;
use crate::args::ChatCommand;
use crate::client::ModelClient;
use crate::config::KaiConfig;
use crate::error::Result;
use crate::ui::{self, CliApprovalPolicy};

/// Executes an interactive conversational REPL loop.
pub async fn execute(_cmd: ChatCommand, config: KaiConfig) -> Result<()> {
    let working_dir = config.canonical_working_dir()?;
    let session_id = format!("session-chat-{}", current_timestamp_ms());

    ui::print_banner(env!("CARGO_PKG_VERSION"), &config.model, &config.base_url);
    println!("Interactive session started. Type your message or command (type '/exit' to quit).\n");

    let inbox = Arc::new(TaskInbox::new(128));
    let client = Arc::new(ModelClient::new(
        &config.base_url,
        &config.model,
        config.api_key.clone(),
    ));

    let tools = default_tools();
    let tool_schemas: Vec<serde_json::Value> = tools.iter().map(|t| t.schema()).collect();

    let agent = Arc::new(Mutex::new(
        LlmAgent::new(
            "kai-chat-agent",
            "KAI Autonomous Engineer",
            &config.system_prompt,
            client,
        )
        .with_tool_schemas(tool_schemas),
    ));

    let event_bus = EventBus::new(128);
    let mut event_rx = event_bus.subscribe();

    let mut engine =
        OrchestrationEngine::new(agent.clone(), inbox.clone(), &working_dir, &session_id)
            .with_max_turns(config.max_turns)
            .with_approval_policy(Arc::new(CliApprovalPolicy::new(config.auto_approve)))
            .with_tool_cache(Arc::new(ToolResultCache::default()))
            .with_event_bus(event_bus);

    for tool in tools {
        engine.register_tool(tool);
    }

    // Spawn telemetry event listener
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
                    ui::print_thought(&format!("Suspended: {reason}"));
                }
                _ => {}
            }
        }
    });

    loop {
        print!("{}{}user>{} ", ui::bold(), ui::green(), ui::reset());
        let _ = io::stdout().flush();

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(err) => {
                ui::print_error(&format!("Failed to read line: {err}"));
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Built-in slash commands
        match trimmed {
            "/exit" | "/quit" => {
                println!("Exiting KAI chat session. Goodbye.");
                break;
            }
            "/clear" => {
                let mut agent_guard = agent.lock().await;
                agent_guard.clear_history();
                println!("Conversation history cleared.");
                continue;
            }
            "/tools" => {
                let _ = crate::commands::tools::execute(crate::args::ToolsCommand { json: false });
                continue;
            }
            "/help" => {
                println!("\nAvailable commands:");
                println!("  /tools - Display all registered tools");
                println!("  /clear - Clear active session conversation history");
                println!("  /exit  - Quit the interactive session\n");
                continue;
            }
            _ => {}
        }

        // Enqueue user message
        let user_msg = Message::user(format!("msg_user_{}", current_timestamp_ms()), trimmed);
        if let Err(err) = inbox.enqueue(user_msg).await {
            ui::print_error(&format!("Failed to enqueue message: {err}"));
            continue;
        }

        // Run engine turns for this user prompt
        let mut turn_count = 0;
        loop {
            turn_count += 1;
            if turn_count > config.max_turns {
                ui::print_error(&format!(
                    "Turn budget exceeded ({} turns). Returning control to user.",
                    config.max_turns
                ));
                break;
            }

            match engine.step().await {
                Ok(StepOutcome::Completed(asst_msg)) => {
                    ui::print_assistant_response(&asst_msg.text_content());
                    break;
                }
                Ok(StepOutcome::Continue(_)) => {
                    // Engine executed tool and enqueued results, proceed to next step
                    continue;
                }
                Ok(StepOutcome::Suspended { reason }) => {
                    ui::print_thought(&format!("Turn suspended: {reason}"));
                    break;
                }
                Err(err) => {
                    ui::print_error(&format!("Engine execution error: {err}"));
                    break;
                }
            }
        }
    }

    Ok(())
}
