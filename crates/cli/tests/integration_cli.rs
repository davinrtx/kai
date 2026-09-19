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
use kai_core::traits::{BoxFuture, Tool};
use kai_core::ToolResultCache;
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

    let tool = Arc::new(ReadWindowTool::new());
    let tool_schemas = vec![tool.schema()];

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
