//! Event bus primitives and live steering signaling.
//!
//! Provides decoupled telemetry via [`tokio::sync::broadcast`] and real-time
//! agent steering via [`tokio::sync::mpsc`] and [`tokio::sync::watch`].

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::error::{KaiError, OrchestratorError, Result};
use crate::message::{ToolCall, ToolResult};

/// Errors occurring during event bus subscription operations.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EventBusError {
    /// The event bus channel has been closed.
    #[error("Event bus channel closed")]
    Closed,

    /// The subscriber lagged behind and dropped messages.
    #[error("Event bus receiver lagged by {skipped} messages")]
    Lagged {
        /// Number of events skipped due to buffer overflow.
        skipped: u64,
    },
}

/// System-wide lifecycle and operational events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// An agent or sub-agent commenced task execution.
    AgentStarted {
        /// Identifier of the active agent.
        agent_id: String,
        /// Objective or task description.
        task: String,
    },
    /// An agent successfully concluded execution.
    AgentCompleted {
        /// Identifier of the completed agent.
        agent_id: String,
    },
    /// An agent encountered an unrecoverable failure during execution.
    AgentError {
        /// Identifier of the affected agent.
        agent_id: String,
        /// Failure details or error description.
        error: String,
    },
    /// An agent initiated a tool invocation.
    ToolInvoked {
        /// Identifier of the requesting agent.
        agent_id: String,
        /// Invocation details.
        tool_call: ToolCall,
    },
    /// A tool invocation completed and returned a result.
    ToolCompleted {
        /// Identifier of the executing agent.
        agent_id: String,
        /// Outcome produced by the tool.
        tool_result: ToolResult,
    },
    /// Context processor completed token reduction or compaction.
    ContextCompacted {
        /// Strategy employed for token compaction (e.g. "ast_skeleton", "sliding_window").
        strategy: String,
        /// Estimated tokens prior to compaction.
        tokens_before: usize,
        /// Estimated tokens after compaction.
        tokens_after: usize,
    },
    /// Agent execution was interrupted or redirected.
    Interrupted {
        /// Identifier of the affected agent.
        agent_id: String,
        /// Reason for interruption.
        reason: String,
    },
    /// A streaming token or text fragment emitted during model inference.
    TokenDelta {
        /// Identifier of the agent streaming content.
        agent_id: String,
        /// Incremental text fragment.
        delta: String,
    },
    /// A sub-agent was dispatched by a parent agent.
    SubAgentSpawned {
        /// Identifier of the parent agent initiating dispatch.
        parent_agent_id: String,
        /// Identifier of the spawned sub-agent.
        sub_agent_id: String,
        /// Task assigned to the sub-agent.
        task: String,
    },
}

/// Central broadcast event bus for decoupled runtime telemetry.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    /// Creates a new [`EventBus`] with the given channel capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publishes an event to all active subscribers.
    ///
    /// Returns the number of subscribers that received the event.
    pub fn publish(&self, event: Event) -> Result<usize> {
        match self.sender.send(event) {
            Ok(receiver_count) => Ok(receiver_count),
            Err(_) => {
                // If there are no active receivers, broadcast::send returns an error.
                // In telemetry buses, zero subscribers is a valid state.
                Ok(0)
            }
        }
    }

    /// Subscribes to the event stream, receiving all subsequent events.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// Receives the next event from a subscriber, automatically recovering from lagged overflow bursts.
    ///
    /// If the receiver lags behind due to bursting telemetry, this helper skips dropped messages
    /// and returns the next fresh event rather than terminating the subscriber loop.
    pub async fn recv_resilient(
        receiver: &mut broadcast::Receiver<Event>,
    ) -> std::result::Result<Event, EventBusError> {
        loop {
            match receiver.recv().await {
                Ok(event) => return Ok(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(EventBusError::Closed);
                }
            }
        }
    }
}

/// Control signal for real-time agent steering without restarting session state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SteeringSignal {
    /// Pause tool and agent execution.
    Pause,
    /// Resume execution of a paused agent.
    Resume,
    /// Cancel the active operation immediately with a reason.
    Cancel {
        /// Explanation for cancellation.
        reason: String,
    },
    /// Redirect the agent's active objective with supplemental guidance.
    Redirect {
        /// New instructions or guidance.
        guidance: String,
    },
}

/// Sender handle for dispatching steering signals to an agent.
pub type SteeringSender = mpsc::Sender<SteeringSignal>;

/// Receiver handle for consuming steering signals within an agent loop.
pub type SteeringReceiver = mpsc::Receiver<SteeringSignal>;

/// Constructs a new asynchronous steering channel with the designated buffer capacity.
pub fn steering_channel(buffer: usize) -> (SteeringSender, SteeringReceiver) {
    mpsc::channel(buffer)
}

/// Global operational state broadcast across all active agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SteeringState {
    /// Active execution is running normally.
    Running,
    /// Execution is globally paused.
    Paused,
    /// Execution is globally terminating or cancelled.
    Terminated,
}

/// Watch sender for broadcasting global agent execution state across sub-agents.
pub type GlobalSteeringSender = watch::Sender<SteeringState>;

/// Watch receiver for monitoring global agent execution state across sub-agents.
pub type GlobalSteeringReceiver = watch::Receiver<SteeringState>;

/// Creates a new global steering watch channel initialized to [`SteeringState::Running`].
pub fn global_steering_channel() -> (GlobalSteeringSender, GlobalSteeringReceiver) {
    watch::channel(SteeringState::Running)
}

/// Helper method to send an interruption error if a cancellation signal is received.
pub fn check_steering_signal(signal: Option<SteeringSignal>) -> Result<Option<SteeringSignal>> {
    match signal {
        Some(SteeringSignal::Cancel { reason }) => {
            Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
                reason,
            }))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus_broadcast() {
        let bus = EventBus::new(16);
        let mut subscriber = bus.subscribe();

        let event = Event::AgentStarted {
            agent_id: "agent-01".to_string(),
            task: "Verify dependencies".to_string(),
        };

        let delivered = bus.publish(event.clone()).unwrap();
        assert_eq!(delivered, 1);

        let received = subscriber.recv().await.unwrap();
        assert_eq!(received, event);
    }

    #[tokio::test]
    async fn test_event_bus_recv_resilient() {
        // Small capacity to trigger lag intentionally
        let bus = EventBus::new(2);
        let mut subscriber = bus.subscribe();

        for i in 0..5 {
            bus.publish(Event::AgentError {
                agent_id: format!("sub_{i}"),
                error: "overflow test".to_string(),
            })
            .unwrap();
        }

        // Subscriber lagged because 5 events were sent on capacity 2.
        // recv_resilient recovers transparently without failing.
        let event = EventBus::recv_resilient(&mut subscriber).await.unwrap();
        match event {
            Event::AgentError { error, .. } => assert_eq!(error, "overflow test"),
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn test_steering_channel_signal() {
        let (tx, mut rx) = steering_channel(8);

        tx.send(SteeringSignal::Pause).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received, SteeringSignal::Pause);

        tx.send(SteeringSignal::Cancel {
            reason: "User abort".to_string(),
        })
        .await
        .unwrap();

        let cancel_signal = rx.recv().await;
        let result = check_steering_signal(cancel_signal);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_global_steering_channel_watch() {
        let (tx, rx1) = global_steering_channel();
        let rx2 = rx1.clone();

        assert_eq!(*rx1.borrow(), SteeringState::Running);
        assert_eq!(*rx2.borrow(), SteeringState::Running);

        tx.send(SteeringState::Paused).unwrap();
        assert_eq!(*rx1.borrow(), SteeringState::Paused);
        assert_eq!(*rx2.borrow(), SteeringState::Paused);

        tx.send(SteeringState::Terminated).unwrap();
        assert_eq!(*rx1.borrow(), SteeringState::Terminated);
        assert_eq!(*rx2.borrow(), SteeringState::Terminated);
    }

    #[tokio::test]
    async fn test_event_bus_high_concurrency_stress() {
        let bus = std::sync::Arc::new(EventBus::new(64));
        let mut subscriber = bus.subscribe();
        let tasks_count = 10;
        let events_per_task = 500;
        let total_events = tasks_count * events_per_task;

        let mut handles = Vec::new();
        for task_idx in 0..tasks_count {
            let bus_clone = bus.clone();
            let handle = tokio::spawn(async move {
                for i in 0..events_per_task {
                    let event = Event::AgentStarted {
                        agent_id: format!("worker_{task_idx}"),
                        task: format!("subtask_{i}"),
                    };
                    let _ = bus_clone.publish(event);
                    if i % 50 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            });
            handles.push(handle);
        }

        let collector_handle = tokio::spawn(async move {
            let mut received_count = 0;
            while let Ok(event) = EventBus::recv_resilient(&mut subscriber).await {
                received_count += 1;
                if let Event::AgentCompleted { agent_id, .. } = event {
                    if agent_id == "sentinel" {
                        break;
                    }
                }
            }
            received_count
        });

        for handle in handles {
            handle.await.unwrap();
        }

        // Sentinel event guarantees the collector breaks out
        bus.publish(Event::AgentCompleted {
            agent_id: "sentinel".to_string(),
        })
        .unwrap();

        let collector_timeout =
            tokio::time::timeout(std::time::Duration::from_secs(3), collector_handle).await;

        assert!(
            collector_timeout.is_ok(),
            "Collector must finish within timeout"
        );
        let count = collector_timeout.unwrap().unwrap();
        assert!(count > 0, "Collector should receive events during burst");
        assert!(count <= total_events + 1);
    }

    #[tokio::test]
    async fn test_steering_signal_multi_consumer_stress() {
        let (tx, rx) = global_steering_channel();
        let workers = 10;
        let mut handles = Vec::new();

        for id in 0..workers {
            let mut rx_clone = rx.clone();
            let handle = tokio::spawn(async move {
                let mut transitions = 0;
                while rx_clone.changed().await.is_ok() {
                    let state = *rx_clone.borrow_and_update();
                    transitions += 1;
                    if state == SteeringState::Terminated {
                        break;
                    }
                }
                (id, transitions)
            });
            handles.push(handle);
        }

        tokio::task::yield_now().await;
        tx.send(SteeringState::Paused).unwrap();
        tokio::task::yield_now().await;
        tx.send(SteeringState::Running).unwrap();
        tokio::task::yield_now().await;
        tx.send(SteeringState::Paused).unwrap();
        tokio::task::yield_now().await;
        tx.send(SteeringState::Terminated).unwrap();

        for handle in handles {
            let (id, transitions) = handle.await.unwrap();
            assert!(transitions >= 1, "worker {id} observed transitions");
        }
    }

    #[tokio::test]
    async fn test_event_streaming_and_subagent_events() {
        let bus = EventBus::new(8);
        let mut sub = bus.subscribe();

        bus.publish(Event::SubAgentSpawned {
            parent_agent_id: "agent_root".to_string(),
            sub_agent_id: "sub_01".to_string(),
            task: "Explore filesystem".to_string(),
        })
        .unwrap();

        bus.publish(Event::TokenDelta {
            agent_id: "sub_01".to_string(),
            delta: "Analyzing...".to_string(),
        })
        .unwrap();

        let ev1 = sub.recv().await.unwrap();
        assert!(matches!(ev1, Event::SubAgentSpawned { .. }));

        let ev2 = sub.recv().await.unwrap();
        if let Event::TokenDelta { agent_id, delta } = ev2 {
            assert_eq!(agent_id, "sub_01");
            assert_eq!(delta, "Analyzing...");
        } else {
            panic!("Expected TokenDelta");
        }

        bus.publish(Event::ContextCompacted {
            strategy: "ast_skeleton".to_string(),
            tokens_before: 12000,
            tokens_after: 3500,
        })
        .unwrap();

        let ev3 = sub.recv().await.unwrap();
        if let Event::ContextCompacted {
            strategy,
            tokens_before,
            tokens_after,
        } = ev3
        {
            assert_eq!(strategy, "ast_skeleton");
            assert_eq!(tokens_before, 12000);
            assert_eq!(tokens_after, 3500);
        } else {
            panic!("Expected ContextCompacted");
        }
    }
}
