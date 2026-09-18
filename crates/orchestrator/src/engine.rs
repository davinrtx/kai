//! Core event-driven orchestration execution engine.
//!
//! Coordinates the cyclic agent turn loop:
//! `Inbox -> Agent Step -> Tool Execution -> Event Bus -> Inbox`
//! with cooperative steering cancellation and pause checks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::event::{Event, EventBus, GlobalSteeringReceiver};
use kai_core::message::{Message, ToolResult};
use kai_core::traits::{Agent, StepOutcome, Tool, ToolContext};
use tokio::sync::Mutex;

use crate::inbox::TaskInbox;

/// Default maximum consecutive reasoning turns permitted in [`OrchestrationEngine::run`].
pub const DEFAULT_MAX_TURNS: usize = 50;

/// Central event-driven execution runtime coordinating an agent and its tool suite.
pub struct OrchestrationEngine {
    agent: Arc<Mutex<dyn Agent>>,
    inbox: Arc<TaskInbox>,
    tools: HashMap<String, Arc<dyn Tool>>,
    event_bus: Option<EventBus>,
    steering: Option<GlobalSteeringReceiver>,
    working_dir: PathBuf,
    session_id: String,
    max_turns: usize,
}

impl OrchestrationEngine {
    /// Constructs a new [`OrchestrationEngine`].
    pub fn new(
        agent: Arc<Mutex<dyn Agent>>,
        inbox: Arc<TaskInbox>,
        working_dir: impl Into<PathBuf>,
        session_id: impl Into<String>,
    ) -> Self {
        Self {
            agent,
            inbox,
            tools: HashMap::new(),
            event_bus: None,
            steering: None,
            working_dir: working_dir.into(),
            session_id: session_id.into(),
            max_turns: DEFAULT_MAX_TURNS,
        }
    }

    /// Attaches an [`EventBus`] for broadcasting lifecycle telemetry.
    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Attaches a [`GlobalSteeringReceiver`] for cooperative cancellation and pause checks.
    pub fn with_steering(mut self, steering: GlobalSteeringReceiver) -> Self {
        self.steering = Some(steering);
        self
    }

    /// Configures the maximum number of consecutive turns allowed per run.
    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns.max(1);
        self
    }

    /// Registers an executable tool into the runtime suite.
    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Retrieves references to all registered tools.
    pub fn tools(&self) -> &HashMap<String, Arc<dyn Tool>> {
        &self.tools
    }

    /// Builds a [`ToolContext`] populated with active engine state and steering signals.
    fn build_tool_context(&self, agent_id: &str) -> ToolContext {
        let mut ctx = ToolContext::new(&self.working_dir, &self.session_id, agent_id);
        if let Some(st) = &self.steering {
            ctx = ctx.with_steering(st.clone());
        }
        ctx
    }

    /// Executes a single discrete reasoning and execution turn.
    pub async fn step(&mut self) -> Result<StepOutcome> {
        // 1. Cooperative steering checks
        if let Some(rx) = &self.steering {
            if *rx.borrow() == kai_core::event::SteeringState::Terminated {
                return Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
                    reason: "Execution cancelled by steering signal".to_string(),
                }));
            }
            if *rx.borrow() == kai_core::event::SteeringState::Paused {
                return Ok(StepOutcome::Suspended {
                    reason: "Execution suspended by steering pause signal".to_string(),
                });
            }
        }

        // 2. Drain pending inbox messages
        let incoming_messages = self.inbox.drain_all().await;

        let agent_id = {
            let guard = self.agent.lock().await;
            guard.id().to_string()
        };

        // 3. Emit step started telemetry
        if let Some(bus) = &self.event_bus {
            let _ = bus.publish(Event::AgentStarted {
                agent_id: agent_id.clone(),
                task: "step".to_string(),
            });
        }

        // 4. Execute agent reasoning step
        let outcome = {
            let mut guard = self.agent.lock().await;
            guard.step(&incoming_messages).await?
        };

        // 5. Handle step outcome and dispatch tool calls
        match &outcome {
            StepOutcome::Continue(messages) => {
                for msg in messages {
                    for call in msg.tool_call_blocks() {
                        // Cooperative steering check before executing each tool call
                        if let Some(rx) = &self.steering {
                            if *rx.borrow() == kai_core::event::SteeringState::Terminated {
                                return Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
                                    reason: "Execution cancelled by steering signal during tool batch".to_string(),
                                }));
                            }
                        }

                        let tool_call_id = call.id.clone();
                        let tool_name = call.name.clone();

                        if let Some(bus) = &self.event_bus {
                            let _ = bus.publish(Event::ToolInvoked {
                                agent_id: agent_id.clone(),
                                tool_call: (*call).clone(),
                            });
                        }

                        let tool_ctx = self.build_tool_context(&agent_id);

                        let tool_result = if let Some(tool) = self.tools.get(&tool_name) {
                            match tool.execute(call.arguments.clone(), &tool_ctx).await {
                                Ok(res) => res,
                                Err(err) => ToolResult::error(&tool_call_id, err.to_string()),
                            }
                        } else {
                            ToolResult::error(
                                &tool_call_id,
                                format!("Tool not found in registry: '{tool_name}'"),
                            )
                        };

                        if let Some(bus) = &self.event_bus {
                            let _ = bus.publish(Event::ToolCompleted {
                                agent_id: agent_id.clone(),
                                tool_result: tool_result.clone(),
                            });
                        }

                        // Enqueue tool result back into the inbox for next reasoning turn
                        let response_msg = Message::tool_results(
                            format!("resp_{tool_call_id}"),
                            vec![tool_result],
                        );
                        self.inbox.enqueue(response_msg).await?;
                    }
                }

                Ok(outcome)
            }
            StepOutcome::Completed(final_msg) => {
                if let Some(bus) = &self.event_bus {
                    let _ = bus.publish(Event::AgentCompleted { agent_id });
                }
                Ok(StepOutcome::Completed(final_msg.clone()))
            }
            StepOutcome::Suspended { reason } => {
                if let Some(bus) = &self.event_bus {
                    let _ = bus.publish(Event::Interrupted {
                        agent_id,
                        reason: reason.clone(),
                    });
                }
                Ok(StepOutcome::Suspended {
                    reason: reason.clone(),
                })
            }
        }
    }

    /// Runs consecutive reasoning turns until completion, suspension, or maximum turn budget.
    pub async fn run(&mut self) -> Result<Message> {
        for _turn in 0..self.max_turns {
            match self.step().await? {
                StepOutcome::Completed(msg) => return Ok(msg),
                StepOutcome::Continue(_) => continue,
                StepOutcome::Suspended { reason } => {
                    return Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
                        reason: format!("Agent execution suspended: {reason}"),
                    }));
                }
            }
        }

        Err(KaiError::Orchestrator(OrchestratorError::Interrupted {
            reason: format!(
                "Execution exceeded maximum turns budget ({} turns) without completing",
                self.max_turns
            ),
        }))
    }
}
