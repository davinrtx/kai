//! Handler for the `kai chat` subcommand (interactive multi-turn REPL).

use std::path::Path;
use std::sync::Arc;

use kai_core::error::KaiError;
use kai_core::event::{Event, EventBus};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::{SessionNode, SessionStore, StepOutcome};
use kai_core::ToolResultCache;
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_session::{AutoCompactor, BranchManager, FileSessionStore, DEFAULT_BRANCH_NAME};
use kai_tools::default_tools;
use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::Editor;
use tokio::sync::Mutex;

use crate::agent::LlmAgent;
use crate::args::ChatCommand;
use crate::client::ModelClient;
use crate::completion::KaiHelper;
use crate::config::KaiConfig;
use crate::error::Result;
use crate::ui::{self, CliApprovalPolicy};

/// Executes an interactive conversational REPL loop.
pub async fn execute(_cmd: ChatCommand, config: KaiConfig) -> Result<()> {
    let working_dir = config.canonical_working_dir()?;
    let kai_dir = working_dir.join(".kai");
    let session_dir = kai_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).map_err(KaiError::Io)?;
    let history_file = kai_dir.join("history");

    let mut session_id = format!("session-chat-{}", current_timestamp_ms());

    let tools = default_tools();
    let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let skills_dir = kai_dir.join("skills");
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

    // Initialize rustyline line editor with Tab completion and persistent history
    let helper = KaiHelper::new(&working_dir, &session_dir);
    let models_handle = helper.discovered_models_handle();
    let mut rl = Editor::<KaiHelper, FileHistory>::new()
        .map_err(|e| KaiError::Io(std::io::Error::other(e.to_string())))?;
    rl.set_helper(Some(helper));
    rl.set_auto_add_history(false);

    if history_file.exists() {
        let _ = rl.load_history(&history_file);
    }

    let probe_client = reqwest::Client::new();
    let discovery_cache = crate::discovery::global_discovery_cache();
    let mut last_discovered: Vec<crate::discovery::DiscoveredModel> = Vec::new();

    // Proactively scan for models in the background to populate Tab completion
    let bg_client = probe_client.clone();
    let bg_cache = discovery_cache.clone();
    let bg_models_handle = models_handle.clone();
    let bg_base_url = config.base_url.clone();
    let bg_api_key = config.api_key.clone();
    tokio::spawn(async move {
        let models = crate::discovery::discover_all_local_models(
            &bg_client,
            Some(&bg_base_url),
            bg_api_key.as_deref(),
            &bg_cache,
        )
        .await;
        if let Ok(mut guard) = bg_models_handle.write() {
            *guard = models.into_iter().map(|m| m.id).collect();
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

        let prompt_str = format!("  {}{}kai ❯{} ", ui::bold(), ui::cyan(), ui::reset());
        let readline = rl.readline(&prompt_str);

        let line = match readline {
            Ok(l) => {
                let _ = rl.add_history_entry(l.as_str());
                l
            }
            Err(ReadlineError::Interrupted) => {
                println!("^C (type /exit to quit)");
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!("Exiting KAI chat session. Goodbye.");
                break;
            }
            Err(err) => {
                ui::print_error(&format!("Readline error: {err}"));
                break;
            }
        };

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
                println!("  /model [name] [endpoint]  - View, switch, or probe inference models and endpoints");
                println!("  /reasoning [on|off]       - Toggle or set internal reasoning (<think>) visibility");
                println!("  /sessions                 - List all saved sessions in .kai/sessions");
                println!("  /resume <id>              - Resume past session ID and restore conversational history");
                println!("  /branch <name>            - Fork or switch active session DAG branch");
                println!(
                    "  /branches                 - List all branches in the active session graph"
                );
                println!(
                    "  /compress | /compact      - Condense past turns in session graph to reduce token usage"
                );
                println!(
                    "  /status                   - View detailed runtime and session telemetry"
                );
                println!("  /tools                    - Display all registered agent tools");
                println!(
                    "  /clear                    - Clear conversation history in active session"
                );
                println!("  /exit                     - Quit the interactive session\n");
                println!("Context Injection:");
                println!("  @path or @file:path       - Attach file contents directly to prompt (bounded to 4 KB)\n");
                continue;
            }
            ["/model"] => {
                let (curr_model, curr_url) = {
                    let a = agent.lock().await;
                    (a.model().to_string(), a.base_url().to_string())
                };
                let is_unconfigured =
                    curr_model == crate::config::UNCONFIGURED_MODEL || curr_model.is_empty();

                if is_unconfigured {
                    println!(
                        "Active model: {}{}[no model configured]{} (endpoint: {})",
                        ui::bold(),
                        ui::red(),
                        ui::reset(),
                        curr_url
                    );
                } else {
                    println!(
                        "Active model: {}{}{} (endpoint: {})",
                        ui::bold(),
                        curr_model,
                        ui::reset(),
                        curr_url
                    );
                }

                println!("\nProbing local endpoints for available models...");
                let discovered = crate::discovery::discover_all_local_models(
                    &probe_client,
                    Some(&curr_url),
                    config.api_key.as_deref(),
                    &discovery_cache,
                )
                .await;

                if discovered.is_empty() {
                    println!(
                        "{}{}[No local models detected on localhost:11434, localhost:1234, or {}]{}",
                        ui::dim(),
                        ui::yellow(),
                        curr_url,
                        ui::reset()
                    );
                    println!(
                        "Configure manually using: {}/model <name> [endpoint]{}",
                        ui::cyan(),
                        ui::reset()
                    );
                    println!(
                        "Or probe a remote endpoint: {}/model probe <endpoint_url>{}\n",
                        ui::cyan(),
                        ui::reset()
                    );
                } else {
                    println!("\nAvailable models discovered:");
                    for (idx, m) in discovered.iter().enumerate() {
                        let num = idx + 1;
                        let desc = m
                            .description
                            .as_deref()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default();
                        let provider_endpoint = format!("[{}] {}", m.provider, m.endpoint);
                        println!(
                            "  {}{:>2}.{} {}{}{} {}{}{}{}",
                            ui::bold(),
                            num,
                            ui::reset(),
                            ui::cyan(),
                            m.id,
                            ui::reset(),
                            ui::dim(),
                            provider_endpoint,
                            desc,
                            ui::reset()
                        );
                    }
                    println!(
                        "\nType {}/model <number>{} or {}/model <name> [endpoint]{} to switch.\n",
                        ui::bold(),
                        ui::reset(),
                        ui::bold(),
                        ui::reset()
                    );

                    if let Ok(mut guard) = models_handle.write() {
                        *guard = discovered.iter().map(|m| m.id.clone()).collect();
                    }
                    last_discovered = discovered;
                }
                continue;
            }
            ["/model", "probe"] => {
                let curr_url = agent.lock().await.base_url().to_string();
                println!("Probing endpoint: {curr_url}...");
                let found = crate::discovery::probe_endpoint(
                    &probe_client,
                    &curr_url,
                    config.api_key.as_deref(),
                    &discovery_cache,
                )
                .await;

                match found {
                    Some(models) if !models.is_empty() => {
                        println!("\nModels found on {curr_url}:");
                        for (idx, m) in models.iter().enumerate() {
                            let num = idx + 1;
                            let desc = m
                                .description
                                .as_deref()
                                .map(|d| format!(" ({d})"))
                                .unwrap_or_default();
                            println!("  {:>2}. {} [{}]{}", num, m.id, m.provider, desc);
                        }
                        println!("\nType '/model <number>' to select one of these models.\n");
                        if let Ok(mut guard) = models_handle.write() {
                            *guard = models.iter().map(|m| m.id.clone()).collect();
                        }
                        last_discovered = models;
                    }
                    _ => {
                        ui::print_error(&format!("No models discovered on endpoint '{curr_url}'."));
                    }
                }
                continue;
            }
            ["/model", "probe", target_url] => {
                println!("Probing endpoint: {target_url}...");
                let found = crate::discovery::probe_endpoint(
                    &probe_client,
                    target_url,
                    config.api_key.as_deref(),
                    &discovery_cache,
                )
                .await;

                match found {
                    Some(models) if !models.is_empty() => {
                        println!("\nModels found on {target_url}:");
                        for (idx, m) in models.iter().enumerate() {
                            let num = idx + 1;
                            let desc = m
                                .description
                                .as_deref()
                                .map(|d| format!(" ({d})"))
                                .unwrap_or_default();
                            println!("  {:>2}. {} [{}]{}", num, m.id, m.provider, desc);
                        }
                        println!("\nType '/model <number>' to select one of these models.\n");
                        if let Ok(mut guard) = models_handle.write() {
                            *guard = models.iter().map(|m| m.id.clone()).collect();
                        }
                        last_discovered = models;
                    }
                    _ => {
                        ui::print_error(&format!(
                            "No models discovered on endpoint '{target_url}'."
                        ));
                    }
                }
                continue;
            }
            ["/model", arg] => {
                // If arg is a numeric index matching a previously discovered model
                if let Ok(idx) = arg.parse::<usize>() {
                    if idx >= 1 && idx <= last_discovered.len() {
                        let selected = &last_discovered[idx - 1];
                        let mut a = agent.lock().await;
                        a.set_model(&selected.id, Some(selected.endpoint.clone()));
                        println!(
                            "Model switched to: {}{}{} (endpoint: {})",
                            ui::bold(),
                            a.model(),
                            ui::reset(),
                            a.base_url()
                        );
                        continue;
                    }
                }

                let mut a = agent.lock().await;
                a.set_model(*arg, None);
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
            ["/compress"] | ["/compact"] => {
                let head = match branch_mgr.active_head().await {
                    Ok(Some(h)) => h,
                    _ => {
                        ui::print_error("Cannot compact: active branch has no head node.");
                        continue;
                    }
                };

                let compactor = AutoCompactor::new().with_max_turns(2).with_keep_recent(2);

                match compactor
                    .compact_branch(session_store.as_ref(), &head)
                    .await
                {
                    Ok(Some(new_leaf)) => {
                        let active_branch = branch_mgr.active_branch().await;
                        let _ = session_store.set_head(&active_branch, &new_leaf.id).await;
                        let _ = session_store.flush_to_disk().await;

                        if let Ok(history_nodes) =
                            session_store.get_branch_history(&new_leaf.id).await
                        {
                            let mut restored = Vec::new();
                            for node in history_nodes {
                                if !node.id.starts_with("root_") {
                                    restored.push(node.message);
                                }
                            }
                            let count = restored.len();
                            agent.lock().await.restore_history(restored);
                            println!(
                                "Session graph compacted successfully (active branch: '{active_branch}', {count} turn nodes retained)."
                            );
                        }
                    }
                    Ok(None) => {
                        println!(
                            "Session history is already compact (insufficient turn depth to condense)."
                        );
                    }
                    Err(err) => {
                        ui::print_error(&format!("Compaction failed: {err}"));
                    }
                }
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

        // Check if inference model is configured
        let is_unconfigured = {
            let a = agent.lock().await;
            a.model() == crate::config::UNCONFIGURED_MODEL || a.model().is_empty()
        };
        if is_unconfigured {
            ui::print_error(
                "No inference model is configured. Run '/model <name> [endpoint]' to configure one.",
            );
            continue;
        }

        // Expand context references (@path or @file:<path>)
        let (full_prompt, attachments) = expand_context_references(trimmed, &working_dir);
        ui::print_user_prompt_preview(trimmed);
        if !attachments.is_empty() {
            ui::print_info(&format!("Attached context: {}", attachments.join(", ")));
        }

        // Enqueue user message
        let user_now = current_timestamp_ms();
        let user_msg = Message::user(format!("msg_user_{user_now}"), full_prompt);
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

    let _ = rl.save_history(&history_file);

    Ok(())
}

/// Expands context references (`@file:<path>` or `@<path>`) in the user input.
///
/// Discovers file path references, reads up to 4 KB per file enforcing strict truncation caps,
/// and appends the file content to the message context.
pub fn expand_context_references(input: &str, working_dir: &Path) -> (String, Vec<String>) {
    let mut attachments = Vec::new();
    let mut context_blocks = Vec::new();

    for word in input.split_whitespace() {
        if let Some(path_str) = word.strip_prefix('@') {
            let clean_path = path_str.strip_prefix("file:").unwrap_or(path_str);
            let target_path = working_dir.join(clean_path);
            if target_path.is_file() {
                if let Ok(bytes) = std::fs::read(&target_path) {
                    const MAX_BYTES: usize = 4096;
                    let (content, truncated) = if bytes.len() > MAX_BYTES {
                        let text = String::from_utf8_lossy(&bytes[..MAX_BYTES]).into_owned();
                        (text, true)
                    } else {
                        (String::from_utf8_lossy(&bytes).into_owned(), false)
                    };

                    let block = if truncated {
                        format!(
                            "\n--- Context File: {clean_path} (first 4 KB) ---\n{content}\n[Truncated: remaining bytes omitted. Refine query]\n--- End Context ---"
                        )
                    } else {
                        format!(
                            "\n--- Context File: {clean_path} ---\n{content}\n--- End Context ---"
                        )
                    };
                    context_blocks.push(block);
                    attachments.push(format!("@{clean_path} ({} bytes)", bytes.len()));
                }
            }
        }
    }

    if context_blocks.is_empty() {
        (input.to_string(), attachments)
    } else {
        let mut full_prompt = input.to_string();
        for block in context_blocks {
            full_prompt.push('\n');
            full_prompt.push_str(&block);
        }
        (full_prompt, attachments)
    }
}
