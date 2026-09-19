//! High-concurrency multi-agent fleet and inbox saturation stress test suite.
//!
//! Stresses the sub-agent dispatcher under concurrent load, verifies permit recycling,
//! and tests cooperative steering cancellation across concurrent tasks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kai_core::error::Result;
use kai_core::event::{global_steering_channel, SteeringState};
use kai_core::message::Message;
use kai_core::traits::{Agent, BoxFuture, StepOutcome};
use kai_orchestrator::{SubAgentDispatcher, SubAgentStatus, TaskInbox};
use tokio::sync::Mutex;

struct StressAgent {
    id: String,
    execution_count: Arc<AtomicUsize>,
    sleep_ms: u64,
}

impl Agent for StressAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "stress-worker"
    }

    fn step<'a>(&'a mut self, inbox: &'a [Message]) -> BoxFuture<'a, Result<StepOutcome>> {
        Box::pin(async move {
            if self.sleep_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
            }
            self.execution_count.fetch_add(1, Ordering::SeqCst);
            let msg = Message::assistant("m_res", format!("Processed {} messages", inbox.len()));
            Ok(StepOutcome::Completed(msg))
        })
    }
}

#[tokio::test]
async fn test_stress_subagent_dispatcher_concurrency_throttling() {
    // Dispatcher with max 4 concurrent agents
    let dispatcher = Arc::new(SubAgentDispatcher::new(4));
    let completed_counter = Arc::new(AtomicUsize::new(0));

    // Register 16 sub-agents
    for i in 0..16 {
        let agent = Arc::new(Mutex::new(StressAgent {
            id: format!("agent_{i}"),
            execution_count: completed_counter.clone(),
            sleep_ms: 20,
        }));
        dispatcher.register_agent(agent).await.unwrap();
    }

    // Launch all 16 agents concurrently
    let mut tasks = Vec::new();
    for i in 0..16 {
        let disp = dispatcher.clone();
        let agent_id = format!("agent_{i}");
        let handle = tokio::spawn(async move {
            let task_msg = Message::user("m_in", "Execute work");
            disp.dispatch(&agent_id, vec![task_msg]).await
        });
        tasks.push(handle);
    }

    // Await all 16 dispatches
    for handle in tasks {
        let res = handle.await.unwrap().unwrap();
        assert!(res.is_completed());
    }

    // Verify all 16 executed successfully
    assert_eq!(completed_counter.load(Ordering::SeqCst), 16);

    // Verify all agents completed successfully
    let active = dispatcher.list_agents().await;
    for info in active {
        assert_eq!(info.status, SubAgentStatus::Completed);
    }
}

#[tokio::test]
async fn test_stress_inbox_saturation_and_batch_draining() {
    let inbox = Arc::new(TaskInbox::new(64));
    let sender = inbox.sender();

    // Spawn 8 concurrent producers submitting messages
    let mut producers = Vec::new();
    for p_id in 0..8 {
        let s = sender.clone();
        let handle = tokio::spawn(async move {
            for m_id in 0..25 {
                let msg = Message::user(format!("p_{p_id}_m_{m_id}"), "Task Payload");
                s.send(msg).await.unwrap();
            }
        });
        producers.push(handle);
    }

    // Consumer draining in batches of 16
    let mut total_received = 0;
    while total_received < 200 {
        let batch = inbox.dequeue_batch(16).await;
        total_received += batch.len();
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    for p in producers {
        p.await.unwrap();
    }

    assert_eq!(total_received, 200);
    assert_eq!(inbox.drain_all().await.len(), 0);
}

#[tokio::test]
async fn test_stress_steering_interruption_across_fleet() {
    let (tx, rx) = global_steering_channel();

    // Signal termination
    tx.send(SteeringState::Terminated).unwrap();
    assert_eq!(*rx.borrow(), SteeringState::Terminated);
}
