use kai_core::{
    check_steering_signal, global_steering_channel, Agent, BoxFuture, ContentBlock, Event,
    EventBus, KaiError, Message, OrchestratorError, PermissionCategory, Result, SessionNode,
    StepOutcome, TokenUsage, Tool, ToolCall, ToolContext, ToolResult, MAX_TOOL_OUTPUT_BYTES,
    TRUNCATION_BYTE_NOTICE,
};
use serde_json::json;

/// Simulated text analysis tool obeying runtime contracts.
struct TextAnalysisTool;

impl Tool for TextAnalysisTool {
    fn name(&self) -> &str {
        "analyze_text"
    }

    fn description(&self) -> &str {
        "Analyzes text input and returns word and character statistics"
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": { "type": "string" }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileRead
    }

    fn timeout(&self) -> Option<std::time::Duration> {
        Some(std::time::Duration::from_secs(5))
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            context.check_cancellation()?;

            let text = arguments
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    KaiError::Tool(kai_core::ToolError::InvalidArguments {
                        name: "analyze_text".to_string(),
                        reason: "Missing text argument".to_string(),
                    })
                })?;

            let char_count = text.chars().count();
            let word_count = text.split_whitespace().count();
            let output = format!("words:{word_count} chars:{char_count}");

            Ok(ToolResult::success("call_analyze", output).with_duration_ms(2))
        })
    }
}

/// Simulated large generator tool to test truncation caps.
struct LargeOutputTool;

impl Tool for LargeOutputTool {
    fn name(&self) -> &str {
        "large_output"
    }

    fn description(&self) -> &str {
        "Generates oversized output to verify truncation"
    }

    fn schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileRead
    }

    fn execute<'a>(
        &'a self,
        _arguments: serde_json::Value,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolResult>> {
        Box::pin(async move {
            let payload = "A".repeat(8192);
            Ok(ToolResult::success("call_large", payload))
        })
    }
}

/// Simulated Agent driving multi-step tool loops.
struct AutonomousAgent {
    id: String,
    name: String,
    step_count: usize,
}

impl AutonomousAgent {
    fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            step_count: 0,
        }
    }
}

impl Agent for AutonomousAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn step<'a>(&'a mut self, inbox: &'a [Message]) -> BoxFuture<'a, Result<StepOutcome>> {
        Box::pin(async move {
            self.step_count += 1;

            if self.step_count == 1 {
                // Determine tool execution based on inbox
                let user_msg = inbox
                    .iter()
                    .find(|m| m.role == kai_core::Role::User)
                    .map(|m| m.text_content())
                    .unwrap_or_default();

                let tool_call =
                    ToolCall::new("call_analyze", "analyze_text", json!({ "text": user_msg }));

                let request_msg = Message::assistant("msg_tool_call", "")
                    .with_block(ContentBlock::ToolUse(tool_call));

                Ok(StepOutcome::Continue(vec![request_msg]))
            } else {
                // Collect tool results from inbox and finalize response
                let tool_result_text = inbox
                    .iter()
                    .filter_map(|m| {
                        m.content.iter().find_map(|b| match b {
                            ContentBlock::ToolResult(res) => Some(res.output.clone()),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("; ");

                let reply = format!("Analysis completed: {tool_result_text}");
                Ok(StepOutcome::Completed(
                    Message::assistant("msg_final", reply)
                        .with_token_usage(TokenUsage::new(45, 15)),
                ))
            }
        })
    }
}

#[tokio::test]
async fn test_full_agent_runtime_loop() {
    let bus = EventBus::new(32);
    let mut event_sub = bus.subscribe();

    let (_steering_tx, steering_rx) = global_steering_channel();
    let tool_ctx = ToolContext::new("/workspace/project", "session_001", "agent_worker")
        .with_steering(steering_rx);

    let tool = TextAnalysisTool;
    let mut agent = AutonomousAgent::new("agent_worker", "TextAnalyzer");

    // 1. Initial user request
    let mut conversation = vec![Message::user(
        "msg_user_01",
        "KAI autonomous agent architecture verified",
    )];

    bus.publish(Event::AgentStarted {
        agent_id: agent.id().to_string(),
        task: "Analyze user text".to_string(),
    })
    .unwrap();

    // 2. Step 1: Agent requests tool call via Continue outcome
    let outcome = agent.step(&conversation).await.unwrap();
    let tool_call = match outcome {
        StepOutcome::Continue(messages) => {
            let msg = messages.into_iter().next().unwrap();
            let call = msg
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::ToolUse(call) => Some(call.clone()),
                    _ => None,
                })
                .unwrap();
            conversation.push(msg);
            call
        }
        _ => panic!("Expected StepOutcome::Continue"),
    };

    assert_eq!(tool_call.name, "analyze_text");
    assert_eq!(tool_call.id, "call_analyze");

    bus.publish(Event::ToolInvoked {
        agent_id: agent.id().to_string(),
        tool_call: tool_call.clone(),
    })
    .unwrap();

    // Validate and execute tool
    assert!(tool.validate_arguments(&tool_call.arguments).is_ok());
    let tool_result = tool.execute(tool_call.arguments, &tool_ctx).await.unwrap();

    assert_eq!(tool_result.output, "words:5 chars:42");
    assert!(!tool_result.is_error);

    bus.publish(Event::ToolCompleted {
        agent_id: agent.id().to_string(),
        tool_result: tool_result.clone(),
    })
    .unwrap();

    // 3. Append tool result message to conversation history
    let result_msg =
        Message::assistant("msg_tool_res", "").with_block(ContentBlock::ToolResult(tool_result));
    conversation.push(result_msg);

    // 4. Step 2: Agent completes response
    let final_outcome = agent.step(&conversation).await.unwrap();
    assert!(final_outcome.is_completed());
    let final_msg = match final_outcome {
        StepOutcome::Completed(msg) => msg,
        _ => panic!("Expected StepOutcome::Completed"),
    };

    assert_eq!(
        final_msg.text_content(),
        "Analysis completed: words:5 chars:42"
    );
    assert_eq!(
        final_msg.token_usage.as_ref().map(|t| t.total_tokens),
        Some(60)
    );

    bus.publish(Event::AgentCompleted {
        agent_id: agent.id().to_string(),
    })
    .unwrap();

    // 5. Construct session DAG checkpoints
    let root_node = SessionNode::root("node_root", conversation[0].clone(), 1726650000000);
    let step1_node = SessionNode::with_parent(
        "node_step1",
        "node_root",
        conversation[1].clone(),
        1726650001000,
    );
    let final_node = SessionNode::with_parent("node_step2", "node_step1", final_msg, 1726650002000);

    assert!(root_node.is_root());
    assert!(root_node.validate().is_ok());
    assert_eq!(step1_node.primary_parent(), Some("node_root"));
    assert!(step1_node.validate().is_ok());
    assert_eq!(final_node.primary_parent(), Some("node_step1"));
    assert!(final_node.validate().is_ok());

    // 6. Verify event stream delivery
    let mut observed_events = Vec::new();
    while let Ok(event) = event_sub.try_recv() {
        observed_events.push(event);
    }
    assert_eq!(observed_events.len(), 4);
}

#[tokio::test]
async fn test_agent_cancellation_via_steering_signal() {
    let (steering_tx, steering_rx) = global_steering_channel();
    let tool_ctx =
        ToolContext::new("/workspace", "session_cancel", "agent_worker").with_steering(steering_rx);

    let tool = TextAnalysisTool;

    // Send termination signal
    steering_tx
        .send(kai_core::SteeringState::Terminated)
        .unwrap();
    assert!(tool_ctx.is_cancelled());

    // Tool execution must fail fast with Interrupted error
    let err = tool
        .execute(json!({ "text": "hello" }), &tool_ctx)
        .await
        .unwrap_err();

    match err {
        KaiError::Orchestrator(OrchestratorError::Interrupted { reason }) => {
            assert_eq!(reason, "Execution cancelled by steering signal");
        }
        other => panic!("Expected OrchestratorError::Interrupted, got {other:?}"),
    }

    // Direct helper check
    let res = check_steering_signal(Some(kai_core::SteeringSignal::Cancel {
        reason: "User manual cancel".to_string(),
    }));
    assert!(res.is_err());
}

#[tokio::test]
async fn test_tool_large_output_truncation_enforcement() {
    let (_tx, rx) = global_steering_channel();
    let tool_ctx =
        ToolContext::new("/workspace", "session_trunc", "agent_worker").with_steering(rx);

    let tool = LargeOutputTool;
    let result = tool.execute(json!({}), &tool_ctx).await.unwrap();

    assert!(result.output.contains(TRUNCATION_BYTE_NOTICE));
    assert!(result.output.len() <= MAX_TOOL_OUTPUT_BYTES + TRUNCATION_BYTE_NOTICE.len());
}
