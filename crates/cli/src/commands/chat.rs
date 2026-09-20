//! Handler for the `kai chat` subcommand (interactive multi-turn REPL).

use std::io::{self, IsTerminal, Write};
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
use crate::config::{KaiConfig, KaiConfigFile};
use crate::error::Result;
use crate::ui::{self, CliApprovalPolicy};

/// Reads a single line of user input from standard input with a prompt.
fn prompt_line(prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut buffer = String::new();
    let _ = io::stdin().read_line(&mut buffer);
    buffer.trim().to_string()
}

const OPENROUTER_MODELS: &[(&str, &str)] = &[
    (
        "nvidia/nemotron-3.5-lightning:free",
        "Nvidia Nemotron 3.5 Lightning (Free tier with tool calling)",
    ),
    (
        "cohere/north-mini-code:free",
        "Cohere North Mini Code (Free coding model with tool calling)",
    ),
    (
        "deepseek/deepseek-chat",
        "DeepSeek V3 (Paid, high speed & cost-effective)",
    ),
    (
        "deepseek/deepseek-r1",
        "DeepSeek R1 (Paid, advanced reasoning)",
    ),
    ("openai/gpt-4o", "OpenAI GPT-4o (Paid, omni multimodal)"),
    (
        "meta-llama/llama-3.3-70b-instruct",
        "Llama 3.3 70B (Paid, open-weights flagship)",
    ),
];

const OPENAI_MODELS: &[(&str, &str)] = &[
    ("gpt-4o", "GPT-4o (Flagship reasoning & multimodal)"),
    ("gpt-4o-mini", "GPT-4o Mini (Fast, lightweight)"),
    ("o1-preview", "OpenAI o1 (Deep reasoning)"),
    ("o1-mini", "OpenAI o1-mini (Fast reasoning)"),
];

const DEEPSEEK_MODELS: &[(&str, &str)] = &[
    (
        "deepseek-chat",
        "DeepSeek V3 (High capability coding & general)",
    ),
    (
        "deepseek-reasoner",
        "DeepSeek R1 (Chain-of-thought reasoning)",
    ),
];

const GROQ_MODELS: &[(&str, &str)] = &[
    (
        "llama-3.3-70b-versatile",
        "Llama 3.3 70B Versatile (Ultra-low latency)",
    ),
    (
        "llama-3.1-8b-instant",
        "Llama 3.1 8B Instant (Ultra-fast response)",
    ),
    (
        "deepseek-r1-distill-llama-70b",
        "DeepSeek R1 Distill 70B (Fast reasoning)",
    ),
    ("mixtral-8x7b-32768", "Mixtral 8x7B (32k context MoE)"),
];

const LOCAL_RECOMMENDED_MODELS: &[(&str, &str)] = &[
    (
        "qwen2.5-coder:7b",
        "Qwen 2.5 Coder 7B (Optimal coding on consumer GPU/CPU)",
    ),
    (
        "llama3.1:8b",
        "Llama 3.1 8B (Versatile general engineering)",
    ),
    (
        "deepseek-coder-v2:16b",
        "DeepSeek Coder V2 16B (Multi-language coding)",
    ),
    (
        "qwen2.5-coder:14b",
        "Qwen 2.5 Coder 14B (High accuracy coding)",
    ),
];

/// Helper to display a numbered list of curated models and prompt the user to pick one or input a custom identifier.
fn select_model_from_options(provider_name: &str, options: &[(&str, &str)]) -> String {
    println!("\nAvailable models for {provider_name}:");
    for (idx, (model_id, desc)) in options.iter().enumerate() {
        println!("  {:>2}. {} - {}", idx + 1, model_id, desc);
    }
    let custom_idx = options.len() + 1;
    println!("  {:>2}. Other / Custom model name\n", custom_idx);

    let sel = prompt_line(&format!("Select model [1-{custom_idx}] (default: 1): "));
    let idx: usize = sel.parse().unwrap_or(1);
    if idx >= 1 && idx <= options.len() {
        options[idx - 1].0.to_string()
    } else if idx == custom_idx {
        let custom = prompt_line("Enter custom model identifier: ");
        if custom.is_empty() {
            options[0].0.to_string()
        } else {
            custom
        }
    } else {
        options[0].0.to_string()
    }
}

/// Sanitizes API key input by stripping CLI command prefixes (/key, /apikey),
/// HTTP headers (Bearer), quotes, and surrounding whitespace.
pub fn sanitize_api_key(input: &str) -> Option<String> {
    let mut s = input.trim().trim_matches('"').trim_matches('\'').trim();
    for prefix in &[
        "/key",
        "/apikey",
        "Bearer",
        "bearer",
        "export API_KEY=",
        "API_KEY=",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim();
        }
    }
    let cleaned = s
        .trim_start_matches('=')
        .trim_start_matches(':')
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim();

    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

/// Helper to select a model from live discovered models or fall back to curated options.
fn select_model_from_catalog(
    provider_name: &str,
    discovered: &[crate::discovery::DiscoveredModel],
    fallback_options: &[(&str, &str)],
) -> String {
    if discovered.is_empty() {
        println!("\nNo live models found or query timed out. Using curated models.");
        return select_model_from_options(provider_name, fallback_options);
    }

    if discovered.len() <= 25 {
        println!(
            "\nAvailable models for {provider_name} ({} discovered):",
            discovered.len()
        );
        for (idx, m) in discovered.iter().enumerate() {
            let desc = m
                .description
                .as_deref()
                .map(|d| format!(" - {d}"))
                .unwrap_or_default();
            println!("  {:>2}. {}{}", idx + 1, m.id, desc);
        }
        let custom_idx = discovered.len() + 1;
        println!("  {:>2}. Other / Custom model name\n", custom_idx);

        let sel = prompt_line(&format!(
            "Select model [1-{custom_idx}] or type model name (default: 1): "
        ));
        if sel.is_empty() {
            return discovered[0].id.clone();
        }
        if let Ok(idx) = sel.parse::<usize>() {
            if idx >= 1 && idx <= discovered.len() {
                return discovered[idx - 1].id.clone();
            } else if idx == custom_idx {
                let custom = prompt_line("Enter custom model identifier: ");
                return if custom.is_empty() {
                    discovered[0].id.clone()
                } else {
                    custom
                };
            }
        }
        // Direct model identifier entered
        return sel;
    }

    // Catalog has > 25 models (e.g. OpenRouter with 400+ models).
    // Prioritize recommended curated models that exist in the live catalog,
    // followed by other discovered models up to 15 entries.
    let mut displayed: Vec<&crate::discovery::DiscoveredModel> = Vec::new();
    for (rec_id, _) in fallback_options {
        if let Some(m) = discovered.iter().find(|m| m.id == *rec_id) {
            displayed.push(m);
        }
    }
    for m in discovered {
        if !displayed.iter().any(|d| d.id == m.id) {
            displayed.push(m);
            if displayed.len() >= 15 {
                break;
            }
        }
    }

    println!(
        "\nDiscovered {} live models from {provider_name}.",
        discovered.len()
    );
    println!("Popular / recommended models:");
    for (idx, m) in displayed.iter().enumerate() {
        let desc = m
            .description
            .as_deref()
            .map(|d| format!(" - {d}"))
            .unwrap_or_default();
        println!("  {:>2}. {}{}", idx + 1, m.id, desc);
    }
    let filter_idx = displayed.len() + 1;
    let custom_idx = displayed.len() + 2;
    println!(
        "  {:>2}. Filter / search all {} discovered models",
        filter_idx,
        discovered.len()
    );
    println!("  {:>2}. Other / Custom model name\n", custom_idx);

    let sel = prompt_line(&format!(
        "Select [1-{custom_idx}] or type model name (default: 1): "
    ));
    if sel.is_empty() {
        return displayed[0].id.clone();
    }
    if let Ok(idx) = sel.parse::<usize>() {
        if idx >= 1 && idx <= displayed.len() {
            return displayed[idx - 1].id.clone();
        } else if idx == filter_idx {
            let filter =
                prompt_line("Enter search term (e.g. claude, deepseek, gpt, qwen, llama): ");
            let filter_lower = filter.to_lowercase();
            let matches: Vec<&crate::discovery::DiscoveredModel> = discovered
                .iter()
                .filter(|m| {
                    m.id.to_lowercase().contains(&filter_lower)
                        || m.description
                            .as_deref()
                            .map(|d| d.to_lowercase().contains(&filter_lower))
                            .unwrap_or(false)
                })
                .take(20)
                .collect();

            if matches.is_empty() {
                println!("No live models matched '{filter}'. Using custom identifier: {filter}");
                return if filter.is_empty() {
                    displayed[0].id.clone()
                } else {
                    filter
                };
            }

            println!("\nMatching models for '{filter}':");
            for (m_idx, m) in matches.iter().enumerate() {
                let desc = m
                    .description
                    .as_deref()
                    .map(|d| format!(" - {d}"))
                    .unwrap_or_default();
                println!("  {:>2}. {}{}", m_idx + 1, m.id, desc);
            }
            let sub_sel = prompt_line(&format!(
                "Select model [1-{}] (default: 1): ",
                matches.len()
            ));
            let sub_idx: usize = sub_sel.parse().unwrap_or(1);
            return if sub_idx >= 1 && sub_idx <= matches.len() {
                matches[sub_idx - 1].id.clone()
            } else {
                matches[0].id.clone()
            };
        } else if idx == custom_idx {
            let custom = prompt_line("Enter custom model identifier: ");
            return if custom.is_empty() {
                displayed[0].id.clone()
            } else {
                custom
            };
        }
    }

    // Direct model name entered
    sel
}

/// Interactive onboarding wizard invoked on first run or when no inference model is configured.
async fn run_onboarding_wizard(
    working_dir: &Path,
    config: &mut KaiConfig,
    probe_client: &reqwest::Client,
    discovery_cache: &crate::discovery::DiscoveryCache,
) {
    println!("\n{}", ui::horizontal_separator());
    println!("             Welcome to KAI (Krill Agent Interface)");
    println!("{}\n", ui::horizontal_separator());
    println!("No inference provider is currently configured.");
    println!("Select an inference provider to get started:\n");
    println!("  1. Local Provider (Ollama, LM Studio, LocalAI)");
    println!("  2. Cloud / Internet (OpenRouter, OpenAI, DeepSeek, Groq)");
    println!("  3. Skip for now (configure manually via /model and /key)\n");

    let choice = prompt_line("Select [1-3] (default: 1): ");
    let choice_str = if choice.is_empty() {
        "1"
    } else {
        choice.as_str()
    };

    match choice_str {
        "1" => {
            println!("\nScanning local endpoints (localhost:11434 and localhost:1234)...");
            let models = crate::discovery::discover_all_local_models(
                probe_client,
                Some(&config.base_url),
                config.api_key.as_deref(),
                discovery_cache,
            )
            .await;

            if !models.is_empty() {
                println!("\nDiscovered local models:");
                for (idx, m) in models.iter().enumerate() {
                    let desc = m
                        .description
                        .as_deref()
                        .map(|d| format!(" ({d})"))
                        .unwrap_or_default();
                    println!("  {:>2}. {} [{}]{}", idx + 1, m.id, m.provider, desc);
                }
                let sel = prompt_line(&format!(
                    "\nSelect model [1-{}] (default: 1): ",
                    models.len()
                ));
                let idx: usize = sel.parse().unwrap_or(1);
                if idx >= 1 && idx <= models.len() {
                    let chosen = &models[idx - 1];
                    config.model = chosen.id.clone();
                    config.base_url = chosen.endpoint.clone();
                } else {
                    let chosen = &models[0];
                    config.model = chosen.id.clone();
                    config.base_url = chosen.endpoint.clone();
                }
            } else {
                println!(
                    "\nNo running local servers detected on localhost:11434 or localhost:1234."
                );
                let url = prompt_line("Enter endpoint URL (default: http://localhost:11434/v1): ");
                config.base_url = if url.is_empty() {
                    crate::config::DEFAULT_BASE_URL.to_string()
                } else {
                    url
                };

                config.model =
                    select_model_from_options("Local / Ollama", LOCAL_RECOMMENDED_MODELS);
            }

            let key_input = prompt_line("Enter API key (press Enter to leave blank for local): ");
            config.api_key = sanitize_api_key(&key_input);

            let mut file_cfg = KaiConfigFile::load(working_dir).unwrap_or_default();
            file_cfg.base_url = Some(config.base_url.clone());
            file_cfg.model = Some(config.model.clone());
            file_cfg.api_key = config.api_key.clone();
            if let Ok(path) = file_cfg.save(working_dir) {
                println!("\nConfiguration saved to {}", path.display());
            }
        }
        "2" => {
            println!("\nSelect Cloud Provider:");
            println!("  1. OpenRouter (https://openrouter.ai/api/v1) [recommended]");
            println!("  2. OpenAI (https://api.openai.com/v1)");
            println!("  3. DeepSeek (https://api.deepseek.com/v1)");
            println!("  4. Groq (https://api.groq.com/openai/v1)");
            println!("  5. Custom Endpoint URL\n");

            let prov_sel = prompt_line("Select [1-5] (default: 1): ");
            let prov_sel_str = if prov_sel.is_empty() {
                "1"
            } else {
                prov_sel.as_str()
            };

            let (prov_name, base_url, fallback_models) = match prov_sel_str {
                "2" => (
                    "OpenAI",
                    "https://api.openai.com/v1".to_string(),
                    OPENAI_MODELS,
                ),
                "3" => (
                    "DeepSeek",
                    "https://api.deepseek.com/v1".to_string(),
                    DEEPSEEK_MODELS,
                ),
                "4" => (
                    "Groq",
                    "https://api.groq.com/openai/v1".to_string(),
                    GROQ_MODELS,
                ),
                "5" => {
                    let url =
                        prompt_line("Enter endpoint URL (default: https://openrouter.ai/api/v1): ");
                    let resolved_url = if url.is_empty() {
                        "https://openrouter.ai/api/v1".to_string()
                    } else {
                        url
                    };
                    ("Custom Provider", resolved_url, OPENROUTER_MODELS)
                }
                _ => (
                    "OpenRouter",
                    "https://openrouter.ai/api/v1".to_string(),
                    OPENROUTER_MODELS,
                ),
            };

            config.base_url = base_url;

            let key_input = prompt_line(&format!(
                "Enter API key for {prov_name} (press Enter to leave blank / skip): "
            ));
            config.api_key = sanitize_api_key(&key_input);

            println!("\nQuerying {prov_name} for available models...");
            let discovered = crate::discovery::probe_endpoint(
                probe_client,
                &config.base_url,
                config.api_key.as_deref(),
                discovery_cache,
            )
            .await
            .unwrap_or_default();

            config.model = select_model_from_catalog(prov_name, &discovered, fallback_models);

            let mut file_cfg = KaiConfigFile::load(working_dir).unwrap_or_default();
            file_cfg.base_url = Some(config.base_url.clone());
            file_cfg.model = Some(config.model.clone());
            file_cfg.api_key = config.api_key.clone();
            if let Ok(path) = file_cfg.save(working_dir) {
                println!("\nConfiguration saved to {}", path.display());
            }
        }
        _ => {
            println!(
                "\nConfiguration skipped. You can configure anytime using '/model <name> [endpoint]' and '/key <token>'."
            );
        }
    }
    println!();
}

/// Executes an interactive conversational REPL loop.
pub async fn execute(_cmd: ChatCommand, mut config: KaiConfig) -> Result<()> {
    let working_dir = config.canonical_working_dir()?;
    let kai_dir = working_dir.join(".kai");
    let session_dir = kai_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).map_err(KaiError::Io)?;
    let history_file = kai_dir.join("history");

    let probe_client = reqwest::Client::new();
    let discovery_cache = crate::discovery::global_discovery_cache();

    let config_file_exists = KaiConfigFile::locate(&working_dir).is_some();
    let is_unconfigured =
        config.model == crate::config::UNCONFIGURED_MODEL || config.model.is_empty();

    if (!config_file_exists || is_unconfigured) && std::io::stdin().is_terminal() {
        run_onboarding_wizard(&working_dir, &mut config, &probe_client, &discovery_cache).await;
    }

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
    let tool_schemas = crate::client::build_tool_schemas(&tools);

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
            .with_compressor(Arc::new(kai_context::SemanticCommandCompressor::new()))
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
        let active_model = {
            let a = agent.lock().await;
            a.model().to_string()
        };
        let git_branch = ui::detect_git_branch(&working_dir);

        // Top horizontal separator line
        ui::signal::ensure_console_mode();
        println!("{}", ui::horizontal_separator());

        let readline = rl.readline("> ");

        // Bottom horizontal separator line
        println!("{}", ui::horizontal_separator());

        // Status footer line: shortcuts on left, status badges on right
        ui::print_prompt_footer(&active_model, git_branch.as_deref(), config.auto_approve);

        let line = match readline {
            Ok(l) => {
                let _ = rl.add_history_entry(l.as_str());
                l
            }
            Err(ReadlineError::Interrupted) => {
                ui::signal::reset();
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
            ["/cancel"] => {
                println!("No active turn is currently running. Press Ctrl+C while waiting for a response to cancel an active turn.");
                continue;
            }
            ["/tools"] => {
                let _ = crate::commands::tools::execute(crate::args::ToolsCommand { json: false });
                continue;
            }
            ["?"] | ["/help"] => {
                println!("\nAvailable commands:");
                println!("  /model [name] [endpoint]  - View, switch, or probe inference models and endpoints");
                println!(
                    "  /model setup              - Run interactive model & provider setup wizard"
                );
                println!("  /key [token|clear]        - View, set, or clear runtime API key");
                println!("  /config                   - View persistent configuration (.kai/config.json)");
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
            ["/config"] => {
                let (model, base_url, api_key_masked) = {
                    let a = agent.lock().await;
                    let masked = match a.api_key() {
                        Some(k) if !k.is_empty() => {
                            if k.len() > 8 {
                                format!("configured ({}...{})", &k[..4], &k[k.len() - 4..])
                            } else {
                                "configured (****)".to_string()
                            }
                        }
                        _ => "unconfigured".to_string(),
                    };
                    (a.model().to_string(), a.base_url().to_string(), masked)
                };
                let cfg_loc = KaiConfigFile::locate(&working_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| {
                        working_dir
                            .join(".kai")
                            .join("config.json")
                            .display()
                            .to_string()
                    });

                println!("\nKAI Configuration ({cfg_loc}):");
                println!("  Base URL:  {base_url}");
                println!("  Model:     {model}");
                println!("  API Key:   {api_key_masked}\n");
                continue;
            }
            ["/key"] | ["/apikey"] => {
                let key_opt = {
                    let a = agent.lock().await;
                    a.api_key().map(String::from)
                };
                match key_opt {
                    Some(k) if !k.is_empty() => {
                        let masked = if k.len() > 8 {
                            format!("{}...{}", &k[..4], &k[k.len() - 4..])
                        } else {
                            "****".to_string()
                        };
                        println!("Active API Key: {masked}");
                    }
                    _ => {
                        println!("Active API Key: [none configured]");
                        println!("Set key using: /key <token> or /key clear");
                    }
                }
                continue;
            }
            ["/key", "clear"] | ["/apikey", "clear"] => {
                let mut a = agent.lock().await;
                a.set_api_key(None);
                println!("API key cleared.");
                let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                file_cfg.api_key = None;
                let _ = file_cfg.save(&working_dir);
                continue;
            }
            _ if trimmed.starts_with("/key") || trimmed.starts_with("/apikey") => {
                let raw = if let Some(rest) = trimmed.strip_prefix("/apikey") {
                    rest
                } else {
                    trimmed.strip_prefix("/key").unwrap_or("")
                };
                let raw_clean = raw
                    .trim()
                    .trim_start_matches('=')
                    .trim_start_matches(':')
                    .trim();
                if raw_clean == "clear" {
                    let mut a = agent.lock().await;
                    a.set_api_key(None);
                    println!("API key cleared.");
                    let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                    file_cfg.api_key = None;
                    let _ = file_cfg.save(&working_dir);
                    continue;
                }
                let clean_key = match sanitize_api_key(raw) {
                    Some(k) => k,
                    None => {
                        println!("Usage: /key <token> or /key clear");
                        continue;
                    }
                };
                let mut a = agent.lock().await;
                a.set_api_key(Some(clean_key.clone()));
                let masked = if clean_key.len() > 8 {
                    format!(
                        "{}...{}",
                        &clean_key[..4],
                        &clean_key[clean_key.len() - 4..]
                    )
                } else {
                    "****".to_string()
                };
                println!("API key configured: {masked}");

                let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                file_cfg.api_key = Some(clean_key);
                if let Ok(path) = file_cfg.save(&working_dir) {
                    println!("Saved API key to {}", path.display());
                }
                continue;
            }
            ["/model"] => {
                let (curr_model, curr_url, curr_key) = {
                    let a = agent.lock().await;
                    (
                        a.model().to_string(),
                        a.base_url().to_string(),
                        a.api_key().map(String::from),
                    )
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
                    curr_key.as_deref().or(config.api_key.as_deref()),
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
                        "Or run interactive wizard: {}/model setup{}",
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
            ["/model", "setup"] => {
                run_onboarding_wizard(&working_dir, &mut config, &probe_client, &discovery_cache)
                    .await;
                let mut a = agent.lock().await;
                a.set_model(&config.model, Some(config.base_url.clone()));
                if let Some(ref k) = config.api_key {
                    a.set_api_key(Some(k.clone()));
                }
                println!(
                    "Model updated to: {}{}{} (endpoint: {})",
                    ui::bold(),
                    a.model(),
                    ui::reset(),
                    a.base_url()
                );
                continue;
            }
            ["/model", "probe"] => {
                let (curr_url, curr_key) = {
                    let a = agent.lock().await;
                    (a.base_url().to_string(), a.api_key().map(String::from))
                };
                println!("Probing endpoint: {curr_url}...");
                let found = crate::discovery::probe_endpoint(
                    &probe_client,
                    &curr_url,
                    curr_key.as_deref().or(config.api_key.as_deref()),
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
                let curr_key = agent.lock().await.api_key().map(String::from);
                println!("Probing endpoint: {target_url}...");
                let found = crate::discovery::probe_endpoint(
                    &probe_client,
                    target_url,
                    curr_key.as_deref().or(config.api_key.as_deref()),
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

                        if a.api_key().is_none()
                            && (!a.base_url().contains("localhost")
                                && !a.base_url().contains("127.0.0.1"))
                        {
                            println!(
                                "No API key is configured for remote endpoint '{}'.",
                                a.base_url()
                            );
                            let key_input =
                                prompt_line("Enter API key (press Enter to leave blank): ");
                            if let Some(clean) = sanitize_api_key(&key_input) {
                                a.set_api_key(Some(clean));
                                println!("API key configured.");
                            }
                        }

                        let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                        file_cfg.model = Some(a.model().to_string());
                        file_cfg.base_url = Some(a.base_url().to_string());
                        file_cfg.api_key = a.api_key().map(String::from);
                        let _ = file_cfg.save(&working_dir);

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

                if a.api_key().is_none()
                    && (!a.base_url().contains("localhost") && !a.base_url().contains("127.0.0.1"))
                {
                    println!(
                        "No API key is configured for remote endpoint '{}'.",
                        a.base_url()
                    );
                    let key_input = prompt_line("Enter API key (press Enter to leave blank): ");
                    if let Some(clean) = sanitize_api_key(&key_input) {
                        a.set_api_key(Some(clean));
                        println!("API key configured.");
                    }
                }

                let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                file_cfg.model = Some(a.model().to_string());
                file_cfg.base_url = Some(a.base_url().to_string());
                file_cfg.api_key = a.api_key().map(String::from);
                let _ = file_cfg.save(&working_dir);

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

                if a.api_key().is_none()
                    && (!a.base_url().contains("localhost") && !a.base_url().contains("127.0.0.1"))
                {
                    println!(
                        "No API key is configured for remote endpoint '{}'.",
                        a.base_url()
                    );
                    let key_input = prompt_line("Enter API key (press Enter to leave blank): ");
                    if let Some(clean) = sanitize_api_key(&key_input) {
                        a.set_api_key(Some(clean));
                        println!("API key configured.");
                    }
                }

                let mut file_cfg = KaiConfigFile::load(&working_dir).unwrap_or_default();
                file_cfg.model = Some(a.model().to_string());
                file_cfg.base_url = Some(a.base_url().to_string());
                file_cfg.api_key = a.api_key().map(String::from);
                let _ = file_cfg.save(&working_dir);

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
                let (model, base_url, api_key_masked, tokens, show_reasoning) = {
                    let a = agent.lock().await;
                    let masked = match a.api_key() {
                        Some(k) if !k.is_empty() => {
                            if k.len() > 8 {
                                format!("configured ({}...{})", &k[..4], &k[k.len() - 4..])
                            } else {
                                "configured (****)".to_string()
                            }
                        }
                        _ => "unconfigured".to_string(),
                    };
                    (
                        a.model().to_string(),
                        a.base_url().to_string(),
                        masked,
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
                println!("  API Key:            {api_key_masked}");
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
        let user_node = if let Some(parent) = &parent_head {
            SessionNode::with_parent(&user_node_id, parent.clone(), user_msg, user_now)
        } else {
            SessionNode::root(&user_node_id, user_msg, user_now)
        };
        let _ = session_store.put_node(&user_node).await;
        let _ = session_store.set_head(&active_branch, &user_node.id).await;

        // Run engine turns for this user prompt
        ui::signal::ensure_console_mode();
        ui::signal::reset();
        let mut sig_rx = ui::signal::subscribe();

        let mut turn_count = 0;
        let mut cancelled = false;
        loop {
            turn_count += 1;
            if turn_count > config.max_turns {
                ui::print_error(&format!(
                    "Turn budget exceeded ({} turns). Returning control to user.",
                    config.max_turns
                ));
                break;
            }

            if ui::signal::was_cancelled() {
                cancelled = true;
            } else {
                let mut step_future = Box::pin(engine.step());
                let step_outcome = tokio::select! {
                    biased;
                    _ = sig_rx.recv() => {
                        cancelled = true;
                        None
                    }
                    _ = tokio::signal::ctrl_c() => {
                        cancelled = true;
                        None
                    }
                    res = &mut step_future => Some(res),
                };

                if step_outcome.is_none() {
                    cancelled = true;
                } else if let Some(res) = step_outcome {
                    match res {
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
                                SessionNode::with_parent(
                                    &asst_node_id,
                                    parent,
                                    asst_msg.clone(),
                                    asst_now,
                                )
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

            if cancelled || ui::signal::was_cancelled() {
                println!("\r                                                                                \r^C [Turn cancelled by user]");
                ui::signal::reset();
                if let Some(prev) = &parent_head {
                    let _ = session_store.set_head(&active_branch, prev).await;
                }
                {
                    let mut a = agent.lock().await;
                    a.pop_last_if_user();
                }
                break;
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
