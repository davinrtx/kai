//! Exhaustive integration test suite for `kai-orchestrator`.
//!
//! Verifies task inbox queuing, sub-agent supervised dispatching with fault isolation,
//! cyclic engine turn execution with tool dispatch and telemetry, steering cancellation/pause,
//! and daemon supervisor lifecycle.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kai_core::event::{global_steering_channel, Event, EventBus, SteeringState};
use kai_core::message::{Message, ToolCall, ToolResult};
use kai_core::traits::{Agent, BoxFuture, PermissionCategory, StepOutcome, Tool, ToolContext};
use kai_orchestrator::{
    DaemonState, DaemonSupervisor, OrchestrationEngine, SubAgentDispatcher, SubAgentStatus,
    TaskInbox,
};
use serde_json::json;
use tokio::sync::Mutex;

/// Mock agent that executes a simple predetermined state machine.
struct MockStateMachineAgent {
    id: String,
    name: String,
    turn_counter: AtomicUsize,
    fail_on_turn: Option<usize>,
}

impl MockStateMachineAgent {
    fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            turn_counter: AtomicUsize::new(0),
            fail_on_turn: None,
        }
    }

    fn failing(id: impl Into<String>, name: impl Into<String>, fail_turn: usize) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            turn_counter: AtomicUsize::new(0),
            fail_on_turn: Some(fail_turn),
        }
    }
}

impl Agent for MockStateMachineAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn step<'a>(
        &'a mut self,
        inbox: &'a [Message],
    ) -> BoxFuture<'a, kai_core::error::Result<StepOutcome>> {
        Box::pin(async move {
            let turn = self.turn_counter.fetch_add(1, Ordering::SeqCst) + 1;

            if let Some(f) = self.fail_on_turn {
                if turn >= f {
                    return Err(kai_core::error::KaiError::Orchestrator(
                        kai_core::error::OrchestratorError::SubAgentFailed {
                            agent_id: self.id.clone(),
                            reason: "Simulated agent internal failure".to_string(),
                        },
                    ));
                }
            }

            // Check if inbox has a tool result
            let has_tool_result = inbox
                .iter()
                .any(|m| m.role == kai_core::message::Role::Tool);

            if has_tool_result {
                // Final turn after tool executed
                Ok(StepOutcome::Completed(Message::assistant(
                    "msg_done",
                    "Task finished with tool output",
                )))
            } else if turn == 1 {
                // First turn: invoke a tool call
                let call = ToolCall::new("call_001", "mock_echo", json!({ "val": "hello_kai" }));
                let msg = Message::tool_calls("msg_1", vec![call]);
                Ok(StepOutcome::Continue(vec![msg]))
            } else {
                Ok(StepOutcome::Completed(Message::assistant(
                    "msg_final",
                    "Completed without tool",
                )))
            }
        })
    }
}

/// Simple echo tool for orchestrator integration testing.
struct MockEchoTool;

impl Tool for MockEchoTool {
    fn name(&self) -> &str {
        "mock_echo"
    }

    fn description(&self) -> &str {
        "Echoes input value"
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "val": { "type": "string" }
            }
        })
    }

    fn permission_category(&self) -> PermissionCategory {
        PermissionCategory::FileRead
    }

    fn execute<'a>(
        &'a self,
        arguments: serde_json::Value,
        _context: &'a ToolContext,
    ) -> BoxFuture<'a, kai_core::error::Result<ToolResult>> {
        Box::pin(async move {
            let val = arguments
                .get("val")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Ok(ToolResult::success("call_001", format!("echo:{val}")))
        })
    }
}

/// Mock slow agent for testing concurrent dispatch and unregistration invariants.
struct MockSlowAgent {
    id: String,
    name: String,
    delay: Duration,
}

impl MockSlowAgent {
    fn new(id: impl Into<String>, name: impl Into<String>, delay: Duration) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            delay,
        }
    }
}

impl Agent for MockSlowAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn step<'a>(
        &'a mut self,
        _inbox: &'a [Message],
    ) -> BoxFuture<'a, kai_core::error::Result<StepOutcome>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(StepOutcome::Completed(Message::assistant(
                "slow_done",
                "completed after sleep",
            )))
        })
    }
}

/// Tool that terminates global steering upon execution.
struct CancellingTool {
    steering_tx: kai_core::event::GlobalSteeringSender,
}

impl CancellingTool {
    fn new(steering_tx: kai_core::event::GlobalSteeringSender) -> Self {
        Self { steering_tx }
    }
}

impl Tool for CancellingTool {
    fn name(&self) -> &str {
        "cancelling_tool"
    }

    fn description(&self) -> &str {
        "Cancels global steering"
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
    ) -> BoxFuture<'a, kai_core::error::Result<ToolResult>> {
        Box::pin(async move {
            let _ = self.steering_tx.send(SteeringState::Terminated);
            Ok(ToolResult::success("call_cancel", "terminated steering"))
        })
    }
}

/// Tool that counts invocations.
struct CountingTool {
    counter: Arc<AtomicUsize>,
}

impl CountingTool {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        Self { counter }
    }
}

impl Tool for CountingTool {
    fn name(&self) -> &str {
        "counting_tool"
    }

    fn description(&self) -> &str {
        "Counts invocations"
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
    ) -> BoxFuture<'a, kai_core::error::Result<ToolResult>> {
        Box::pin(async move {
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::success("call_count", "counted"))
        })
    }
}

#[tokio::test]
async fn test_task_inbox_queue_and_batch_draining() {
    let inbox = TaskInbox::new(10);

    // 1. Enqueue messages
    for i in 1..=5 {
        inbox
            .enqueue(Message::user(format!("msg_{i}"), format!("Content {i}")))
            .await
            .unwrap();
    }

    // 2. Batch dequeue 3 items
    let batch = inbox.dequeue_batch(3).await;
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0].text_content(), "Content 1");
    assert_eq!(batch[2].text_content(), "Content 3");

    // 3. Drain remaining 2 items without blocking
    let remaining = inbox.drain_all().await;
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].text_content(), "Content 4");
    assert_eq!(remaining[1].text_content(), "Content 5");

    // 4. Drain on empty returns empty vec
    let empty = inbox.drain_all().await;
    assert!(empty.is_empty());
}

#[tokio::test]
async fn test_subagent_dispatcher_lifecycle_and_fault_isolation() {
    let dispatcher = SubAgentDispatcher::new(4);

    let agent_healthy = Arc::new(Mutex::new(MockStateMachineAgent::new(
        "agent_healthy",
        "worker",
    )));
    let agent_flaky = Arc::new(Mutex::new(MockStateMachineAgent::failing(
        "agent_flaky",
        "flaky_worker",
        1,
    )));

    // 1. Register agents
    let id_h = dispatcher.register_agent(agent_healthy).await.unwrap();
    let id_f = dispatcher.register_agent(agent_flaky).await.unwrap();

    assert_eq!(dispatcher.agent_count().await, 2);
    let list = dispatcher.list_agents().await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "agent_flaky");
    assert_eq!(list[1].id, "agent_healthy");

    // 2. Dispatch to healthy agent succeeds
    let outcome = dispatcher.dispatch(&id_h, Vec::new()).await.unwrap();
    assert!(outcome.is_continue());

    let info_h = dispatcher.get_agent_info(&id_h).await.unwrap();
    assert_eq!(info_h.status, SubAgentStatus::Idle);

    // 3. Dispatch to flaky agent fails with isolated error, without crashing dispatcher
    let fail_res = dispatcher.dispatch(&id_f, Vec::new()).await;
    assert!(fail_res.is_err());

    let info_f = dispatcher.get_agent_info(&id_f).await.unwrap();
    match info_f.status {
        SubAgentStatus::Failed(err) => {
            assert!(err.contains("Simulated agent internal failure"));
        }
        _ => panic!("Expected failed status"),
    }

    // 4. Unregister agent
    assert!(dispatcher.unregister_agent(&id_f).await.is_ok());
    assert_eq!(dispatcher.agent_count().await, 1);
    assert!(dispatcher.unregister_agent("non_existent").await.is_err());
}

#[tokio::test]
async fn test_orchestration_engine_full_turn_loop_with_tools_and_events() {
    let bus = EventBus::new(32);
    let mut event_rx = bus.subscribe();

    let inbox = Arc::new(TaskInbox::new(10));
    let agent = Arc::new(Mutex::new(MockStateMachineAgent::new(
        "main_agent",
        "orchestrator_tester",
    )));

    let mut engine = OrchestrationEngine::new(agent, inbox.clone(), "/workspace", "sess_orch_1")
        .with_event_bus(bus.clone());

    engine.register_tool(Arc::new(MockEchoTool));

    // Turn 1: Agent produces tool call `mock_echo` -> Engine executes tool and enqueues ToolResult
    let outcome1 = engine.step().await.unwrap();
    assert!(outcome1.is_continue());

    // Verify event bus captured step and tool telemetry, but NOT AgentCompleted yet!
    let mut turn1_events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        turn1_events.push(evt);
    }
    assert!(turn1_events
        .iter()
        .any(|e| matches!(e, Event::AgentStarted { .. })));
    assert!(turn1_events
        .iter()
        .any(|e| matches!(e, Event::ToolInvoked { .. })));
    assert!(turn1_events
        .iter()
        .any(|e| matches!(e, Event::ToolCompleted { .. })));
    // Invariant: AgentCompleted must NOT be emitted during intermediate Continue turns!
    assert!(!turn1_events
        .iter()
        .any(|e| matches!(e, Event::AgentCompleted { .. })));

    // Verify inbox now holds the tool result
    let pending_msgs = inbox.drain_all().await;
    assert_eq!(pending_msgs.len(), 1);
    assert_eq!(pending_msgs[0].role, kai_core::message::Role::Tool);
    let tool_res = pending_msgs[0].tool_result_blocks();
    assert_eq!(tool_res.len(), 1);
    assert!(tool_res[0].output.contains("echo:hello_kai"));

    // Put it back so agent consumes it in Turn 2
    inbox
        .enqueue(pending_msgs.into_iter().next().unwrap())
        .await
        .unwrap();

    // Turn 2: Agent consumes ToolResult and completes
    let outcome2 = engine.step().await.unwrap();
    assert!(outcome2.is_completed());
    assert_eq!(
        outcome2.as_completed().unwrap().text_content(),
        "Task finished with tool output"
    );

    // Verify event bus captured AgentCompleted upon completion
    let mut turn2_events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        turn2_events.push(evt);
    }
    assert!(turn2_events
        .iter()
        .any(|e| matches!(e, Event::AgentCompleted { .. })));
}

#[tokio::test]
async fn test_orchestration_engine_steering_cancellation_and_pause() {
    let (steering_tx, steering_rx) = global_steering_channel();

    let inbox = Arc::new(TaskInbox::new(10));
    let agent = Arc::new(Mutex::new(MockStateMachineAgent::new(
        "steered_agent",
        "steered_worker",
    )));

    let mut engine = OrchestrationEngine::new(agent, inbox, "/workspace", "sess_steer")
        .with_steering(steering_rx);

    // 1. Pause signal suspends engine
    steering_tx.send(SteeringState::Paused).unwrap();
    let pause_outcome = engine.step().await.unwrap();
    assert!(pause_outcome.is_suspended());

    // 2. Terminated signal halts with Interrupted error
    steering_tx.send(SteeringState::Terminated).unwrap();
    let cancel_res = engine.step().await;
    assert!(cancel_res.is_err());
    let err_msg = cancel_res.unwrap_err().to_string();
    assert!(err_msg.contains("cancelled by steering signal") || err_msg.contains("Interrupted"));
}

#[tokio::test]
async fn test_daemon_supervisor_lifecycle_and_shutdown() {
    let inbox = Arc::new(TaskInbox::new(10));
    let agent = Arc::new(Mutex::new(MockStateMachineAgent::new(
        "daemon_agent",
        "daemon_worker",
    )));

    let engine = OrchestrationEngine::new(agent, inbox, "/workspace", "sess_daemon");
    let supervisor = Arc::new(DaemonSupervisor::new(engine, Duration::from_millis(20)));

    assert_eq!(supervisor.state().await, DaemonState::Initializing);

    let supervisor_clone = supervisor.clone();
    let handle = tokio::spawn(async move { supervisor_clone.run().await });

    // Allow daemon to spin up
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(supervisor.state().await, DaemonState::Running);

    let hb1 = supervisor.last_heartbeat_ms();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let hb2 = supervisor.last_heartbeat_ms();
    assert!(hb2 >= hb1);

    // Request graceful shutdown
    supervisor.request_shutdown().await.unwrap();
    let join_res = handle.await.unwrap();
    assert!(join_res.is_ok());

    assert_eq!(supervisor.state().await, DaemonState::Stopped);
}

#[tokio::test]
async fn test_subagent_dispatcher_rejects_concurrent_dispatch_and_unregister_while_running() {
    let dispatcher = SubAgentDispatcher::new(4);
    let slow_agent = Arc::new(Mutex::new(MockSlowAgent::new(
        "slow_worker",
        "slow_role",
        Duration::from_millis(100),
    )));

    let agent_id = dispatcher.register_agent(slow_agent).await.unwrap();

    let dispatcher_clone = dispatcher.clone();
    let agent_id_clone = agent_id.clone();
    let slow_handle =
        tokio::spawn(async move { dispatcher_clone.dispatch(&agent_id_clone, Vec::new()).await });

    // Let the first dispatch start executing
    tokio::time::sleep(Duration::from_millis(20)).await;

    // 1. Status is Running
    let info = dispatcher.get_agent_info(&agent_id).await.unwrap();
    assert_eq!(info.status, SubAgentStatus::Running);

    // 2. Concurrent dispatch on same agent must be rejected
    let concurrent_res = dispatcher.dispatch(&agent_id, Vec::new()).await;
    assert!(concurrent_res.is_err());
    let err = concurrent_res.unwrap_err().to_string();
    assert!(err.contains("already executing a turn"));

    // 3. Unregistering while running must be rejected
    let unregister_res = dispatcher.unregister_agent(&agent_id).await;
    assert!(unregister_res.is_err());
    let unregister_err = unregister_res.unwrap_err().to_string();
    assert!(unregister_err.contains("while it is running"));

    // Wait for the slow dispatch to conclude
    let join_res = slow_handle.await.unwrap();
    assert!(join_res.is_ok());

    // 4. After completion, unregistering succeeds
    assert!(dispatcher.unregister_agent(&agent_id).await.is_ok());
    assert_eq!(dispatcher.agent_count().await, 0);
}

#[tokio::test]
async fn test_orchestration_engine_steering_cancellation_during_multi_tool_execution() {
    let (steering_tx, steering_rx) = global_steering_channel();
    let inbox = Arc::new(TaskInbox::new(10));

    // Agent requesting two tools in a single turn: first cancels steering, second counts
    struct MultiToolAgent;
    impl Agent for MultiToolAgent {
        fn id(&self) -> &str {
            "multi_tool_agent"
        }
        fn name(&self) -> &str {
            "multi_tool"
        }
        fn step<'a>(
            &'a mut self,
            _inbox: &'a [Message],
        ) -> BoxFuture<'a, kai_core::error::Result<StepOutcome>> {
            Box::pin(async move {
                let call1 = ToolCall::new("call_cancel", "cancelling_tool", json!({}));
                let call2 = ToolCall::new("call_count", "counting_tool", json!({}));
                let msg = Message::tool_calls("multi_calls", vec![call1, call2]);
                Ok(StepOutcome::Continue(vec![msg]))
            })
        }
    }

    let mut engine = OrchestrationEngine::new(
        Arc::new(Mutex::new(MultiToolAgent)),
        inbox,
        "/workspace",
        "sess_multi",
    )
    .with_steering(steering_rx);

    let count = Arc::new(AtomicUsize::new(0));
    engine.register_tool(Arc::new(CancellingTool::new(steering_tx)));
    engine.register_tool(Arc::new(CountingTool::new(count.clone())));

    // Step should execute the first tool (which sets steering to Terminated),
    // and abort before executing the second tool.
    let step_res = engine.step().await;
    assert!(step_res.is_err());
    let err_msg = step_res.unwrap_err().to_string();
    assert!(err_msg.contains("during tool batch") || err_msg.contains("Interrupted"));

    // Second tool must NOT have executed
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_task_inbox_try_send_backpressure_error() {
    let inbox = TaskInbox::new(2);
    assert_eq!(inbox.capacity(), 2);
    assert!(!inbox.is_closed());

    // Fill capacity
    inbox.try_enqueue(Message::user("m1", "one")).unwrap();
    inbox.try_enqueue(Message::user("m2", "two")).unwrap();

    // Exceed capacity: try_enqueue must fail with backpressure error
    let overflow_res = inbox.try_enqueue(Message::user("m3", "three"));
    assert!(overflow_res.is_err());
    let err_msg = overflow_res.unwrap_err().to_string();
    assert!(err_msg.contains("backpressure capacity reached"));

    // Close inbox and verify is_closed
    inbox.close().await;
    assert!(inbox.is_closed());
}

#[tokio::test]
async fn test_daemon_supervisor_pausing_state_reflection() {
    let inbox = Arc::new(TaskInbox::new(10));

    // Agent that returns Suspended
    struct SuspendingAgent;
    impl Agent for SuspendingAgent {
        fn id(&self) -> &str {
            "suspending_agent"
        }
        fn name(&self) -> &str {
            "suspender"
        }
        fn step<'a>(
            &'a mut self,
            _inbox: &'a [Message],
        ) -> BoxFuture<'a, kai_core::error::Result<StepOutcome>> {
            Box::pin(async move {
                Ok(StepOutcome::Suspended {
                    reason: "Need user confirmation".to_string(),
                })
            })
        }
    }

    let engine = OrchestrationEngine::new(
        Arc::new(Mutex::new(SuspendingAgent)),
        inbox,
        "/workspace",
        "sess_pause",
    );
    let supervisor = Arc::new(DaemonSupervisor::new(engine, Duration::from_millis(20)));

    let supervisor_clone = supervisor.clone();
    let handle = tokio::spawn(async move { supervisor_clone.run().await });

    // Allow daemon to tick and execute step
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Supervisor state should reflect Paused
    assert_eq!(supervisor.state().await, DaemonState::Paused);

    // Shutdown cleans up cleanly
    supervisor.request_shutdown().await.unwrap();
    let join_res = handle.await.unwrap();
    assert!(join_res.is_ok());
    assert_eq!(supervisor.state().await, DaemonState::Stopped);
}
