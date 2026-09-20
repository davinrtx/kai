//! Integration tests for `kai-cli` verifying argument parsing,
//! protocol serialization, and agent orchestration strictly offline.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use clap::Parser;
use kai_cli::args::{CliArgs, Commands};
use kai_cli::client::{LlmTransport, ModelClient};
use kai_cli::config::KaiConfig;
use kai_cli::error::Result;
use kai_cli::LlmAgent;
use kai_core::message::{Message, ToolCall, ToolResult};
use kai_core::{BoxFuture, Tool, ToolResultCache};
use kai_orchestrator::engine::OrchestrationEngine;
use kai_orchestrator::inbox::TaskInbox;
use kai_tools::ReadWindowTool;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// In-memory mock transport delivering canned responses sequentially.
struct MockTransport {
    responses: Mutex<Vec<Value>>,
    calls: AtomicUsize,
}

impl MockTransport {
    fn new(responses: Vec<Value>) -> Self {
        Self {
            responses: Mutex::new(responses),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LlmTransport for MockTransport {
    fn send_request<'a>(
        &'a self,
        _url: &'a str,
        _api_key: Option<&'a str>,
        _payload: &'a Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.responses.lock().await;
            if guard.is_empty() {
                return Err(kai_cli::error::CliError::Configuration(
                    "MockTransport: No canned responses remaining".to_string(),
                ));
            }
            Ok(guard.remove(0))
        })
    }
}

#[test]
fn test_cli_args_parsing_run() {
    let args = CliArgs::try_parse_from(["kai", "run", "Refactor session DAG", "-y", "-t", "20"]);
    assert!(args.is_ok());
    let parsed = args.unwrap();

    assert!(parsed.yes);
    assert_eq!(parsed.max_turns, Some(20));

    match parsed.command {
        Some(Commands::Run(cmd)) => {
            assert_eq!(cmd.task, "Refactor session DAG");
        }
        _ => panic!("Expected Run subcommand"),
    }
}

#[test]
fn test_cli_args_parsing_shorthand_prompt() {
    let args = CliArgs::try_parse_from([
        "kai",
        "-p",
        "Audit dependencies",
        "--model",
        "custom-model:latest",
    ]);
    assert!(args.is_ok());
    let parsed = args.unwrap();

    assert_eq!(parsed.prompt, Some("Audit dependencies".to_string()));
    assert_eq!(parsed.model, Some("custom-model:latest".to_string()));
    assert!(parsed.command.is_none());
}

#[test]
fn test_cli_args_parsing_tools_json() {
    let args = CliArgs::try_parse_from(["kai", "tools", "--json"]);
    assert!(args.is_ok());
    let parsed = args.unwrap();

    match parsed.command {
        Some(Commands::Tools(cmd)) => {
            assert!(cmd.json);
        }
        _ => panic!("Expected Tools subcommand"),
    }
}

#[test]
fn test_config_resolution_defaults_and_overrides() {
    let cfg = KaiConfig::resolve(
        Some("http://10.0.0.1:8000/v1".to_string()),
        Some("my-model".to_string()),
        Some("secret-key".to_string()),
        Some(PathBuf::from(".")),
        Some(10),
        true,
    )
    .unwrap();

    assert_eq!(cfg.base_url, "http://10.0.0.1:8000/v1");
    assert_eq!(cfg.model, "my-model");
    assert_eq!(cfg.api_key, Some("secret-key".to_string()));
    assert_eq!(cfg.max_turns, 10);
    assert!(cfg.auto_approve);
}

#[test]
fn test_kai_config_file_persistence() {
    let temp_dir = std::env::temp_dir().join(format!(
        "kai_cfg_test_{}",
        kai_core::message::current_timestamp_ms()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let file_cfg = kai_cli::config::KaiConfigFile {
        base_url: Some("https://openrouter.ai/api/v1".to_string()),
        model: Some("anthropic/claude-3.5-sonnet".to_string()),
        api_key: Some("sk-or-test-persisted".to_string()),
        max_turns: Some(30),
        auto_approve: Some(true),
    };

    let path = file_cfg.save(&temp_dir).unwrap();
    assert!(path.exists());

    let loaded = kai_cli::config::KaiConfigFile::load(&temp_dir).unwrap();
    assert_eq!(loaded, file_cfg);

    // Verify KaiConfig::resolve loads from file
    let resolved =
        KaiConfig::resolve(None, None, None, Some(temp_dir.clone()), None, false).unwrap();
    assert_eq!(resolved.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(resolved.model, "anthropic/claude-3.5-sonnet");
    assert_eq!(resolved.api_key, Some("sk-or-test-persisted".to_string()));
    assert_eq!(resolved.max_turns, 30);
    assert!(resolved.auto_approve);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_model_client_message_formatting() {
    let msg_user = Message::user("msg_01", "Hello assistant");
    let call = ToolCall::new(
        "call_01",
        "read_window",
        json!({ "path": "Cargo.toml", "offset": 1 }),
    );
    let msg_asst = Message::tool_calls("msg_02", vec![call]);
    let msg_tool = Message::tool_results(
        "msg_03",
        vec![ToolResult::success("call_01", "[package]\nname = \"kai\"")],
    );

    let messages = vec![msg_user, msg_asst, msg_tool];
    let formatted = ModelClient::format_messages("You are KAI.", &messages);

    assert_eq!(formatted.len(), 4); // system + user + assistant + tool
    assert_eq!(formatted[0]["role"], "system");
    assert_eq!(formatted[0]["content"], "You are KAI.");
    assert_eq!(formatted[1]["role"], "user");
    assert_eq!(formatted[2]["role"], "assistant");
    assert!(formatted[2]["tool_calls"].is_array());
    assert_eq!(formatted[3]["role"], "tool");
    assert_eq!(formatted[3]["tool_call_id"], "call_01");
}

#[tokio::test]
async fn test_offline_mock_model_client_and_agent_execution() {
    let temp_dir = std::env::temp_dir().join(format!(
        "kai_test_cli_{}",
        kai_core::message::current_timestamp_ms()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let test_file = temp_dir.join("sample.txt");
    std::fs::write(&test_file, "Line 1: Alpha\nLine 2: Beta\nLine 3: Gamma\n").unwrap();

    // Turn 1 response: Model requests tool call to read_window
    let resp1 = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "I will read the sample file.",
                "tool_calls": [{
                    "id": "call_rw_1",
                    "type": "function",
                    "function": {
                        "name": "read_window",
                        "arguments": json!({
                            "path": test_file.to_str().unwrap(),
                            "offset": 1,
                            "limit": 5
                        }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });

    // Turn 2 response: Model returns completed answer
    let resp2 = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "The file contains Alpha, Beta, and Gamma.",
                "tool_calls": []
            },
            "finish_reason": "stop"
        }]
    });

    let mock_transport = Arc::new(MockTransport::new(vec![resp1, resp2]));
    let client = Arc::new(ModelClient::with_transport(
        mock_transport.clone(),
        "http://mock-endpoint/v1",
        "mock-model",
        None,
    ));

    let tool: Arc<dyn Tool> = Arc::new(ReadWindowTool::new());
    let tool_schemas = kai_cli::client::build_tool_schemas(std::slice::from_ref(&tool));

    let agent = Arc::new(Mutex::new(
        LlmAgent::new("agent-test", "Test Agent", "System Prompt", client)
            .with_tool_schemas(tool_schemas),
    ));

    let inbox = Arc::new(TaskInbox::new(32));
    let mut engine =
        OrchestrationEngine::new(agent.clone(), inbox.clone(), &temp_dir, "session-cli-test")
            .with_max_turns(5)
            .with_tool_cache(Arc::new(ToolResultCache::default()));

    engine.register_tool(tool);

    // Enqueue initial user prompt
    inbox
        .enqueue(Message::user(
            "msg_u1",
            "Read the file and tell me contents",
        ))
        .await
        .unwrap();

    // Run engine to completion
    let final_outcome = engine.run().await;
    assert!(final_outcome.is_ok());
    let outcome = final_outcome.unwrap();

    assert_eq!(
        outcome.text_content(),
        "The file contains Alpha, Beta, and Gamma."
    );
    assert_eq!(mock_transport.call_count(), 2);

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_detect_git_branch() {
    let temp_dir = std::env::temp_dir().join(format!(
        "kai_git_test_{}",
        kai_core::message::current_timestamp_ms()
    ));
    let git_dir = temp_dir.join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();

    // Test symbolic ref branch
    let head_file = git_dir.join("HEAD");
    std::fs::write(&head_file, "ref: refs/heads/feat/cli-test\n").unwrap();
    let detected = kai_cli::ui::detect_git_branch(&temp_dir);
    assert_eq!(detected, Some("feat/cli-test".to_string()));

    // Test detached commit SHA (40 hex chars)
    std::fs::write(&head_file, "4b825dc642cb6eb9a060e54bf8d69288fbee4904\n").unwrap();
    let detected_detached = kai_cli::ui::detect_git_branch(&temp_dir);
    assert_eq!(detected_detached, Some("4b825dc".to_string()));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_parse_reasoning_blocks() {
    // Standard closed thinking tag
    let raw = "<think>\nStep 1: Check inputs\nStep 2: Calculate\n</think>\nThe answer is 42.";
    let (thought, clean) = kai_cli::ui::parse_reasoning_blocks(raw);
    assert_eq!(
        thought,
        Some("Step 1: Check inputs\nStep 2: Calculate".to_string())
    );
    assert_eq!(clean, "The answer is 42.");

    // Unclosed thinking tag
    let unclosed = "<think>Still reasoning...";
    let (thought_unclosed, clean_unclosed) = kai_cli::ui::parse_reasoning_blocks(unclosed);
    assert_eq!(thought_unclosed, Some("Still reasoning...".to_string()));
    assert_eq!(clean_unclosed, "");

    // Multiple thinking tags
    let multi = "<think>Part 1</think>Middle text<think>Part 2</think>End text";
    let (thought_multi, clean_multi) = kai_cli::ui::parse_reasoning_blocks(multi);
    assert_eq!(thought_multi, Some("Part 1\n\nPart 2".to_string()));
    assert_eq!(clean_multi, "Middle textEnd text");

    // No thinking tags
    let plain = "Plain response without tags.";
    let (thought_plain, clean_plain) = kai_cli::ui::parse_reasoning_blocks(plain);
    assert!(thought_plain.is_none());
    assert_eq!(clean_plain, plain);
}

#[test]
fn test_model_hot_switching() {
    let mock_transport = Arc::new(MockTransport::new(vec![]));
    let client = Arc::new(ModelClient::with_transport(
        mock_transport.clone(),
        "http://localhost:11434/v1",
        "qwen2.5-coder:7b",
        None,
    ));

    let mut agent = LlmAgent::new("test-agent", "Tester", "System", client);
    assert_eq!(agent.model(), "qwen2.5-coder:7b");
    assert_eq!(agent.base_url(), "http://localhost:11434/v1");

    // Switch model only
    agent.set_model("deepseek-coder:6.7b", None);
    assert_eq!(agent.model(), "deepseek-coder:6.7b");
    assert_eq!(agent.base_url(), "http://localhost:11434/v1");

    // Switch model and endpoint
    agent.set_model("gpt-4o", Some("https://api.openai.com/v1".to_string()));
    assert_eq!(agent.model(), "gpt-4o");
    assert_eq!(agent.base_url(), "https://api.openai.com/v1");

    // Dynamic API key switching
    assert!(agent.api_key().is_none());
    agent.set_api_key(Some("sk-test-runtime-key".to_string()));
    assert_eq!(agent.api_key(), Some("sk-test-runtime-key"));
    agent.set_api_key(None);
    assert!(agent.api_key().is_none());

    // Reasoning state toggles
    assert!(!agent.show_reasoning());
    agent.set_show_reasoning(true);
    assert!(agent.show_reasoning());
}

#[test]
fn test_config_multi_provider_env_keys() {
    std::env::set_var("OPENROUTER_API_KEY", "sk-or-test-provider-mock");
    let cfg = KaiConfig::resolve(None, None, None, Some(PathBuf::from(".")), None, false).unwrap();
    assert_eq!(cfg.api_key, Some("sk-or-test-provider-mock".to_string()));
    std::env::remove_var("OPENROUTER_API_KEY");
}

#[test]
fn test_token_usage_parsing_and_accumulation() {
    let raw_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Hello from mock model"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150
        }
    });

    let parsed = ModelClient::parse_response(&raw_response).unwrap();
    assert_eq!(parsed.text, Some("Hello from mock model".to_string()));
    assert!(parsed.usage.is_some());
    let usage = parsed.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 30);
    assert_eq!(usage.total_tokens, 150);

    let mock_transport = Arc::new(MockTransport::new(vec![]));
    let client = Arc::new(ModelClient::with_transport(
        mock_transport,
        "http://localhost:11434/v1",
        "test-model",
        None,
    ));

    let mut agent = LlmAgent::new("test-agent", "Tester", "System", client);
    assert_eq!(agent.accumulated_tokens(), 0);

    agent.record_tokens(150);
    assert_eq!(agent.last_turn_tokens(), 150);
    assert_eq!(agent.accumulated_tokens(), 150);

    agent.record_tokens(50);
    assert_eq!(agent.last_turn_tokens(), 50);
    assert_eq!(agent.accumulated_tokens(), 200);
}

#[tokio::test]
async fn test_session_branching_and_resume() {
    use kai_core::traits::{SessionNode, SessionStore};
    use kai_session::{BranchManager, FileSessionStore, DEFAULT_BRANCH_NAME};

    let temp_dir = std::env::temp_dir().join(format!(
        "kai_session_test_{}",
        kai_core::message::current_timestamp_ms()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let session_id = "test-chat-session";
    let store = Arc::new(FileSessionStore::new(&temp_dir, session_id, true).unwrap());
    let branch_mgr = Arc::new(BranchManager::new(store.clone()));

    // Create root node
    let root_node = SessionNode::root(
        "root_1",
        Message::system("msg_root", "Session initialized"),
        1000,
    );
    store.put_node(&root_node).await.unwrap();
    store.set_head(DEFAULT_BRANCH_NAME, "root_1").await.unwrap();

    assert_eq!(branch_mgr.active_branch().await, "main");
    assert_eq!(
        branch_mgr.active_head().await.unwrap(),
        Some("root_1".to_string())
    );

    // Fork and switch to "feature-exp"
    branch_mgr.fork_branch("main", "feature-exp").await.unwrap();
    branch_mgr.switch_branch("feature-exp").await.unwrap();
    assert_eq!(branch_mgr.active_branch().await, "feature-exp");

    // Add turn node to "feature-exp"
    let turn_node = SessionNode::with_parent(
        "node_turn_1",
        "root_1",
        Message::user("msg_u1", "Testing branch fork"),
        1050,
    );
    store.put_node(&turn_node).await.unwrap();
    store.set_head("feature-exp", "node_turn_1").await.unwrap();

    // Verify branches
    let branches = store.list_branches().await.unwrap();
    assert!(branches.contains(&"main".to_string()));
    assert!(branches.contains(&"feature-exp".to_string()));

    // Verify history on feature-exp
    let history = store.get_branch_history("node_turn_1").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id, "root_1");
    assert_eq!(history[1].id, "node_turn_1");

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_ui_box_panel_rendering() {
    kai_cli::ui::set_color_enabled(false);
    let lines = vec!["Hello world".to_string(), "Second line".to_string()];
    let panel = kai_cli::ui::draw_box_panel("KAI Test", &lines, 40, "", "");
    assert!(panel.contains("KAI Test"));
    assert!(panel.contains("Hello world"));
    assert!(panel.contains('╭'));
    assert!(panel.contains('╰'));
}

#[test]
fn test_ui_wrap_text() {
    let text = "This is a long sentence that should wrap gracefully across multiple lines.";
    let wrapped = kai_cli::ui::wrap_text(text, 20);
    assert!(wrapped.len() >= 3);
    for line in wrapped {
        assert!(line.len() <= 20);
    }
}

#[test]
fn test_unconfigured_model_default() {
    let cfg = KaiConfig::resolve(None, None, None, Some(PathBuf::from(".")), None, true).unwrap();
    assert_eq!(cfg.model, kai_cli::config::UNCONFIGURED_MODEL);
}

#[test]
fn test_context_reference_expansion() {
    let temp_dir = std::env::temp_dir().join(format!(
        "kai_ctx_test_{}",
        kai_core::message::current_timestamp_ms()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();

    let small_file = temp_dir.join("small.txt");
    std::fs::write(&small_file, "Hello from small context file").unwrap();

    let (expanded, attachments) = kai_cli::commands::chat::expand_context_references(
        "Please examine @small.txt and report back",
        &temp_dir,
    );

    assert_eq!(attachments.len(), 1);
    assert!(attachments[0].starts_with("@small.txt"));
    assert!(expanded.contains("Hello from small context file"));
    assert!(expanded.contains("--- Context File: small.txt ---"));

    // Large file exceeding 4 KB cap
    let large_file = temp_dir.join("large.txt");
    let large_content = "x".repeat(5000);
    std::fs::write(&large_file, large_content).unwrap();

    let (expanded_large, attachments_large) =
        kai_cli::commands::chat::expand_context_references("Check @large.txt", &temp_dir);
    assert_eq!(attachments_large.len(), 1);
    assert!(expanded_large.contains("[Truncated: remaining bytes omitted. Refine query]"));

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_visible_width_calculation() {
    assert_eq!(kai_cli::ui::visible_width("plain text"), 10);
    assert_eq!(
        kai_cli::ui::visible_width("\x1b[1m\x1b[31mbold red\x1b[0m"),
        8
    );
}

#[test]
fn test_strip_endpoint_suffixes() {
    use kai_cli::discovery::strip_endpoint_suffixes;

    assert_eq!(
        strip_endpoint_suffixes("http://localhost:11434/v1"),
        "http://localhost:11434"
    );
    assert_eq!(
        strip_endpoint_suffixes("http://localhost:11434/api/tags"),
        "http://localhost:11434"
    );
    assert_eq!(
        strip_endpoint_suffixes("http://localhost:1234/api/v1/models/"),
        "http://localhost:1234"
    );
    assert_eq!(
        strip_endpoint_suffixes("https://api.openai.com/v1/chat/completions"),
        "https://api.openai.com"
    );
}

#[test]
fn test_ollama_tags_json_parsing() {
    use kai_cli::discovery::parse_ollama_tags_json;

    let payload = json!({
        "models": [
            {
                "name": "qwen2.5-coder:7b",
                "model": "qwen2.5-coder:7b",
                "modified_at": "2024-11-20T10:00:00Z",
                "size": 4683072512u64,
                "details": {
                    "family": "qwen2",
                    "parameter_size": "7.6B",
                    "quantization_level": "Q4_K_M"
                }
            },
            {
                "name": "llama3.1:8b",
                "model": "llama3.1:8b",
                "details": {
                    "family": "llama",
                    "parameter_size": "8.0B"
                }
            }
        ]
    });

    let models = parse_ollama_tags_json(&payload, "http://localhost:11434");
    assert_eq!(models.len(), 2);

    assert_eq!(models[0].id, "qwen2.5-coder:7b");
    assert_eq!(models[0].provider, "ollama");
    assert_eq!(models[0].endpoint, "http://localhost:11434/v1");
    assert_eq!(
        models[0].description.as_deref(),
        Some("7.6B, Q4_K_M, qwen2")
    );

    assert_eq!(models[1].id, "llama3.1:8b");
    assert_eq!(models[1].provider, "ollama");
    assert_eq!(models[1].endpoint, "http://localhost:11434/v1");
    assert_eq!(models[1].description.as_deref(), Some("8.0B, llama"));
}

#[test]
fn test_lmstudio_models_json_parsing_and_filter() {
    use kai_cli::discovery::parse_lmstudio_models_json;

    let payload = json!({
        "data": [
            {
                "id": "deepseek-coder-6.7b-instruct",
                "object": "model",
                "type": "llm"
            },
            {
                "id": "bge-large-en-v1.5",
                "object": "model",
                "type": "embeddings"
            },
            {
                "id": "qwen2.5-coder-7b",
                "object": "model"
            }
        ]
    });

    let models = parse_lmstudio_models_json(&payload, "http://localhost:1234");
    assert_eq!(models.len(), 2); // bge embeddings model must be filtered out!

    assert_eq!(models[0].id, "deepseek-coder-6.7b-instruct");
    assert_eq!(models[0].provider, "lmstudio");
    assert_eq!(models[0].endpoint, "http://localhost:1234/v1");

    assert_eq!(models[1].id, "qwen2.5-coder-7b");
    assert_eq!(models[1].provider, "lmstudio");
    assert_eq!(models[1].endpoint, "http://localhost:1234/v1");
}

#[test]
fn test_openai_models_json_parsing() {
    use kai_cli::discovery::parse_openai_models_json;

    let payload = json!({
        "data": [
            {
                "id": "gpt-4o",
                "owned_by": "openai"
            },
            {
                "id": "text-embedding-3-small",
                "owned_by": "openai"
            },
            {
                "id": "claude-3-5-sonnet",
                "owned_by": "anthropic"
            }
        ]
    });

    let models = parse_openai_models_json(&payload, "https://api.openai.com/v1", "openai");
    assert_eq!(models.len(), 2); // text-embedding must be filtered out by heuristic

    assert_eq!(models[0].id, "gpt-4o");
    assert_eq!(models[0].description.as_deref(), Some("openai"));
    assert_eq!(models[1].id, "claude-3-5-sonnet");
}

#[tokio::test]
async fn test_discovery_cache_positive_and_negative() {
    use kai_cli::discovery::{DiscoveredModel, DiscoveryCache};

    let cache = DiscoveryCache::new();
    let url = "http://localhost:11434";

    assert!(!cache.is_negatively_cached(url).await);
    assert!(cache.get(url).await.is_none());

    // Insert positive model
    let model = DiscoveredModel {
        id: "test-model".to_string(),
        provider: "test".to_string(),
        endpoint: format!("{url}/v1"),
        description: None,
    };
    cache.insert(url, vec![model.clone()]).await;

    let cached = cache.get(url).await;
    assert!(cached.is_some());
    assert_eq!(cached.unwrap(), vec![model]);

    // Negative failure caching
    let dead_url = "http://localhost:9999";
    cache.mark_failure(dead_url).await;
    assert!(cache.is_negatively_cached(dead_url).await);

    // Clear cache
    cache.clear().await;
    assert!(!cache.is_negatively_cached(dead_url).await);
    assert!(cache.get(url).await.is_none());
}

#[test]
fn test_sanitize_api_key_variations() {
    use kai_cli::commands::chat::sanitize_api_key;

    assert_eq!(
        sanitize_api_key("sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("/key sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("/apikey sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("/key:sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("/key=sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("Bearer sk-or-v1-2b2803d8"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("\"sk-or-v1-2b2803d8\""),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(
        sanitize_api_key("'/key sk-or-v1-2b2803d8'"),
        Some("sk-or-v1-2b2803d8".to_string())
    );
    assert_eq!(sanitize_api_key(""), None);
    assert_eq!(sanitize_api_key("   "), None);
    assert_eq!(sanitize_api_key("/key"), None);
    assert_eq!(sanitize_api_key("/apikey"), None);
}

#[test]
fn test_parse_openrouter_models_json_with_name() {
    use kai_cli::discovery::parse_openai_models_json;

    let payload = json!({
        "data": [
            {
                "id": "anthropic/claude-3.5-sonnet",
                "name": "Anthropic: Claude 3.5 Sonnet",
                "description": "State of the art reasoning and coding model"
            },
            {
                "id": "text-embedding-ada-002",
                "name": "Embedding Model"
            }
        ]
    });

    let models = parse_openai_models_json(&payload, "https://openrouter.ai/api/v1", "openrouter");
    assert_eq!(models.len(), 1); // embedding model excluded
    assert_eq!(models[0].id, "anthropic/claude-3.5-sonnet");
    assert_eq!(models[0].endpoint, "https://openrouter.ai/api/v1");
    assert_eq!(
        models[0].description.as_deref(),
        Some("Anthropic: Claude 3.5 Sonnet")
    );
}

#[test]
fn test_filter_models_without_tools_support() {
    use kai_cli::discovery::parse_openai_models_json;

    let payload = json!({
        "data": [
            {
                "id": "qwen/qwen-2.5-coder-32b-instruct",
                "name": "Qwen 2.5 Coder 32B",
                "supported_parameters": ["temperature", "max_tokens"]
            },
            {
                "id": "deepseek/deepseek-chat",
                "name": "DeepSeek V3",
                "supported_parameters": ["temperature", "tools", "max_tokens"]
            },
            {
                "id": "unspecified/model-without-params",
                "name": "Legacy model"
            }
        ]
    });

    let models = parse_openai_models_json(&payload, "https://openrouter.ai/api/v1", "openrouter");
    // qwen is excluded because supported_parameters does not contain "tools"
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "deepseek/deepseek-chat");
    assert_eq!(models[1].id, "unspecified/model-without-params");
}

#[test]
fn test_format_tools_has_function_name() {
    use kai_cli::client::{build_tool_schemas, ModelClient};

    let tool: Arc<dyn Tool> = Arc::new(ReadWindowTool::new());
    let tool_schemas = build_tool_schemas(std::slice::from_ref(&tool));
    let formatted = ModelClient::format_tools(&tool_schemas);

    assert_eq!(formatted.len(), 1);
    assert_eq!(formatted[0]["type"], "function");
    assert_eq!(formatted[0]["function"]["name"], "read_window");
    assert!(formatted[0]["function"]["description"].is_string());
    assert!(formatted[0]["function"]["parameters"]["properties"].is_object());
}

struct FallbackMockTransport {
    calls: AtomicUsize,
}

impl LlmTransport for FallbackMockTransport {
    fn send_request<'a>(
        &'a self,
        _url: &'a str,
        _api_key: Option<&'a str>,
        payload: &'a Value,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if payload.get("tools").is_some() {
                return Err(kai_cli::error::CliError::Api {
                    status: 404,
                    message: "No endpoints found that support tool use".to_string(),
                });
            }
            Ok(json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Conversational reply without tools"
                    },
                    "finish_reason": "stop"
                }]
            }))
        })
    }
}

#[tokio::test]
async fn test_model_client_fallback_when_tools_unsupported() {
    let transport = Arc::new(FallbackMockTransport {
        calls: AtomicUsize::new(0),
    });
    let client = ModelClient::with_transport(
        transport.clone(),
        "https://openrouter.ai/api/v1",
        "mock-non-tool-model",
        None,
    );
    let tool: Arc<dyn Tool> = Arc::new(ReadWindowTool::new());
    let tool_schemas = kai_cli::client::build_tool_schemas(std::slice::from_ref(&tool));

    let messages = vec![kai_core::message::Message::user("msg-1", "Hello")];
    let response = client
        .complete("System prompt", &messages, &tool_schemas)
        .await
        .expect("should fallback and succeed");

    assert_eq!(
        response.text.as_deref(),
        Some("Conversational reply without tools")
    );
    assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
}
