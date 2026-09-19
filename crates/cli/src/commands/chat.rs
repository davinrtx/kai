//! Handler for the `kai chat` subcommand (interactive multi-turn REPL).

use std::io::{self, Write};
use std::sync::Arc;

use kai_core::error::KaiError;
use kai_core::event::{Event, EventBus};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::{SessionNode, SessionStore, StepOutcome};
use kai_core::ToolResultCache;
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_session::{BranchManager, FileSessionStore, DEFAULT_BRANCH_NAME};
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
    let session_dir = working_dir.join(".kai").join("sessions");
    std::fs::create_dir_all(&session_dir).map_err(KaiError::Io)?;

    let mut session_id = format!("session-chat-{}", current_timestamp_ms());

    ui::print_banner(env!("CARGO_PKG_VERSION"), &config.model, &config.base_url);
    println!(
        "Interactive session started. Type your message or command (type '/help' for options).\n"
    );

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

    // Initialize session store and branch coordinator
    let mut session_store = Arc::new(FileSessionStore::new(&session_dir, &session_id, true)?);
    let mut branch_mgr = Arc::new(BranchManager::new(session_store.clone()));

    // Record initial root checkpoint into session graph
    let root_id = format!("root_{session_id}");
    if session_store.get_node(&root_id).await?.is_none() {
        let now = current_timestamp_ms();
        let root_node = SessionNode::root(
            &root_id,
            Message::system(format!("msg_{root_id}"), "Session initialized"),
            now,
        );
        session_store.put_node(&root_node).await?;
        session_store
            .set_head(DEFAULT_BRANCH_NAME, &root_id)
            .await?;
        session_store.flush_to_disk().await?;
    }

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
        // Render turn-based status bar
        let (active_model, accumulated_tokens) = {
            let a = agent.lock().await;
            (a.model().to_string(), a.accumulated_tokens())
        };
        let git_branch = ui::detect_git_branch(&working_dir);
        let session_branch = branch_mgr.active_branch().await;

        ui::print_status_bar(
            &active_model,
            accumulated_tokens,
            git_branch.as_deref(),
            &session_branch,
        );

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

        let parts: Vec<&str> = trimmed.split_whitespace().collect();

        // Built-in slash commands
        match parts.as_slice() {
            ["/exit"] | ["/quit"] => {
                println!("Exiting KAI chat session. Goodbye.");
                break;
            }
            ["/clear"] => {
                let mut agent_guard = agent.lock().await;
                agent_guard.clear_history();
                println!("Conversation history cleared.");
                continue;
            }
            ["/tools"] => {
                let _ = crate::commands::tools::execute(crate::args::ToolsCommand { json: false });
                continue;
            }
            ["/help"] => {
                println!("\nAvailable commands:");
                println!("  /model [name] [endpoint]  - View or switch active inference model and endpoint");
                println!("  /reasoning [on|off]       - Toggle or set internal reasoning (<think>) visibility");
                println!("  /sessions                 - List all saved sessions in .kai/sessions");
                println!("  /resume <id>              - Resume past session ID and restore conversational history");
                println!("  /branch <name>            - Fork or switch active session DAG branch");
                println!(
                    "  /branches                 - List all branches in the active session graph"
                );
                println!(
                    "  /status                   - View detailed runtime and session telemetry"
                );
                println!("  /tools                    - Display all registered agent tools");
                println!(
                    "  /clear                    - Clear conversation history in active session"
                );
                println!("  /exit                     - Quit the interactive session\n");
                continue;
            }
            ["/model"] => {
                let a = agent.lock().await;
                println!(
                    "Active model: {}{}{} (endpoint: {})",
                    ui::bold(),
                    a.model(),
                    ui::reset(),
                    a.base_url()
                );
                continue;
            }
            ["/model", new_model] => {
                let mut a = agent.lock().await;
                a.set_model(*new_model, None);
                println!(
                    "Model switched to: {}{}{} (endpoint: {})",
                    ui::bold(),
                    a.model(),
                    ui::reset(),
                    a.base_url()
                );
                continue;
            }
            ["/model", new_model, new_endpoint] => {
                let mut a = agent.lock().await;
                a.set_model(*new_model, Some((*new_endpoint).to_string()));
                println!(
                    "Model switched to: {}{}{} (endpoint: {})",
                    ui::bold(),
                    a.model(),
                    ui::reset(),
                    a.base_url()
                );
                continue;
            }
            ["/reasoning"] => {
                let mut a = agent.lock().await;
                let new_state = !a.show_reasoning();
                a.set_show_reasoning(new_state);
                let state_str = if new_state { "ENABLED" } else { "DISABLED" };
                println!(
                    "Reasoning trace visibility: {}{state_str}{}",
                    ui::bold(),
                    ui::reset()
                );
                continue;
            }
            ["/reasoning", "on"] => {
                let mut a = agent.lock().await;
                a.set_show_reasoning(true);
                println!(
                    "Reasoning trace visibility: {}ENABLED{}",
                    ui::bold(),
                    ui::reset()
                );
                continue;
            }
            ["/reasoning", "off"] => {
                let mut a = agent.lock().await;
                a.set_show_reasoning(false);
                println!(
                    "Reasoning trace visibility: {}DISABLED{}",
                    ui::bold(),
                    ui::reset()
                );
                continue;
            }
            ["/sessions"] => {
                match session_store.list_sessions().await {
                    Ok(mut sessions) => {
                        sessions.sort();
                        println!("\nAvailable Persistent Sessions (.kai/sessions):");
                        if sessions.is_empty() {
                            println!("  (no saved sessions found)");
                        } else {
                            for s in sessions {
                                let mark = if s == session_id { "*" } else { " " };
                                println!("  {mark} {s}");
                            }
                        }
                        println!();
                    }
                    Err(err) => {
                        ui::print_error(&format!("Failed to list sessions: {err}"));
                    }
                }
                continue;
            }
            ["/resume", target_id] => {
                let target_file = session_dir.join(format!("{target_id}.json"));
                if !target_file.exists() {
                    ui::print_error(&format!(
                        "Session '{target_id}' not found at {}",
                        target_file.display()
                    ));
                    continue;
                }

                match FileSessionStore::new(&session_dir, *target_id, true) {
                    Ok(new_store) => {
                        let new_store = Arc::new(new_store);
                        let new_branch_mgr = Arc::new(BranchManager::new(new_store.clone()));
                        let head = match new_branch_mgr.active_head().await {
                            Ok(Some(h)) => h,
                            _ => match new_store.list_branches().await {
                                Ok(branches) if !branches.is_empty() => new_store
                                    .get_head(&branches[0])
                                    .await
                                    .unwrap_or(None)
                                    .unwrap_or_default(),
                                _ => String::new(),
                            },
                        };

                        let mut restored_messages = Vec::new();
                        if !head.is_empty() {
                            if let Ok(history_nodes) = new_store.get_branch_history(&head).await {
                                for node in history_nodes {
                                    if !node.id.starts_with("root_") {
                                        restored_messages.push(node.message);
                                    }
                                }
                            }
                        }

                        let count = restored_messages.len();
                        agent.lock().await.restore_history(restored_messages);
                        session_store = new_store;
                        branch_mgr = new_branch_mgr;
                        session_id = (*target_id).to_string();

                        let active_branch = branch_mgr.active_branch().await;
                        println!(
                            "Resumed session '{session_id}' ({count} turn messages loaded, active branch: '{active_branch}')."
                        );
                    }
                    Err(err) => {
                        ui::print_error(&format!("Failed to load session '{target_id}': {err}"));
                    }
                }
                continue;
            }
            ["/branch", name] => {
                let current_branch = branch_mgr.active_branch().await;
                if *name == current_branch {
                    println!("Already on branch '{name}'.");
                    continue;
                }

                let head = branch_mgr.active_head().await.unwrap_or(None);
                if let Some(_head_node_id) = head {
                    let branch_exists =
                        session_store.get_head(name).await.unwrap_or(None).is_some();
                    if branch_exists {
                        if let Err(err) = branch_mgr.switch_branch(name).await {
                            ui::print_error(&format!("Failed to switch branch: {err}"));
                        } else {
                            println!("Switched to branch '{name}'.");
                        }
                    } else if let Err(err) = branch_mgr.fork_branch(&current_branch, name).await {
                        ui::print_error(&format!("Failed to fork branch: {err}"));
                    } else if let Err(err) = branch_mgr.switch_branch(name).await {
                        ui::print_error(&format!("Failed to switch branch: {err}"));
                    } else {
                        println!(
                            "Created and switched to branch '{name}' (forked from '{current_branch}')."
                        );
                    }
                } else {
                    ui::print_error("Cannot create branch: active branch has no head node.");
                }
                continue;
            }
            ["/branches"] => {
                match session_store.list_branches().await {
                    Ok(branches) => {
                        let active = branch_mgr.active_branch().await;
                        println!("\nSession DAG Branches:");
                        for b in branches {
                            let head = session_store.get_head(&b).await.unwrap_or(None);
                            let head_str = head.as_deref().unwrap_or("<empty>");
                            if b == active {
                                println!("  * {}{b}{} (head: {head_str})", ui::bold(), ui::reset());
                            } else {
                                println!("    {b} (head: {head_str})");
                            }
                        }
                        println!();
                    }
                    Err(err) => {
                        ui::print_error(&format!("Failed to list branches: {err}"));
                    }
                }
                continue;
            }
            ["/status"] => {
                let (model, base_url, tokens, show_reasoning) = {
                    let a = agent.lock().await;
                    (
                        a.model().to_string(),
                        a.base_url().to_string(),
                        a.accumulated_tokens(),
                        a.show_reasoning(),
                    )
                };
                let git_branch_str =
                    ui::detect_git_branch(&working_dir).unwrap_or_else(|| "none".to_string());
                let active_branch = branch_mgr.active_branch().await;
                let head = branch_mgr
                    .active_head()
                    .await
                    .unwrap_or(None)
                    .unwrap_or_else(|| "none".to_string());

                println!("\nKAI Session Status:");
                println!("  Model:              {model}");
                println!("  Endpoint:           {base_url}");
                println!("  Git Branch:         {git_branch_str}");
                println!("  Session ID:         {session_id}");
                println!("  DAG Branch:         {active_branch}");
                println!("  DAG Head Node:      {head}");
                println!("  Accumulated Tokens: {tokens}");
                println!(
                    "  Reasoning Display:  {}\n",
                    if show_reasoning {
                        "ENABLED"
                    } else {
                        "DISABLED"
                    }
                );
                continue;
            }
            _ => {
                if trimmed.starts_with('/') {
                    ui::print_error(&format!(
                        "Unknown slash command: '{trimmed}'. Type '/help' for available commands."
                    ));
                    continue;
                }
            }
        }

        // Enqueue user message
        let user_now = current_timestamp_ms();
        let user_msg = Message::user(format!("msg_user_{user_now}"), trimmed);
        if let Err(err) = inbox.enqueue(user_msg.clone()).await {
            ui::print_error(&format!("Failed to enqueue message: {err}"));
            continue;
        }

        // Record user message into session graph
        let active_branch = branch_mgr.active_branch().await;
        let parent_head = branch_mgr.active_head().await.unwrap_or(None);
        let user_node_id = format!("node_{user_now}");
        let user_node = if let Some(parent) = parent_head {
            SessionNode::with_parent(&user_node_id, parent, user_msg, user_now)
        } else {
            SessionNode::root(&user_node_id, user_msg, user_now)
        };
        let _ = session_store.put_node(&user_node).await;
        let _ = session_store.set_head(&active_branch, &user_node.id).await;

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
                    let full_text = asst_msg.text_content();
                    let (thought, clean_text) = ui::parse_reasoning_blocks(&full_text);

                    let show_reasoning = agent.lock().await.show_reasoning();
                    if show_reasoning {
                        if let Some(ref t) = thought {
                            ui::print_thought(t);
                        }
                    }

                    if !clean_text.is_empty() {
                        ui::print_assistant_response(&clean_text);
                    } else if thought.is_some() && !show_reasoning {
                        ui::print_assistant_response(
                            "[Completed internal reasoning. Type '/reasoning on' to view traces.]",
                        );
                    }

                    // Record assistant message into session graph
                    let asst_now = current_timestamp_ms();
                    let parent_head = branch_mgr.active_head().await.unwrap_or(None);
                    let asst_node_id = format!("node_{asst_now}");
                    let asst_node = if let Some(parent) = parent_head {
                        SessionNode::with_parent(&asst_node_id, parent, asst_msg.clone(), asst_now)
                    } else {
                        SessionNode::root(&asst_node_id, asst_msg.clone(), asst_now)
                    };
                    let _ = session_store.put_node(&asst_node).await;
                    let _ = session_store.set_head(&active_branch, &asst_node.id).await;
                    let _ = session_store.flush_to_disk().await;
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
