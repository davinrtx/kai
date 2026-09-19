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
use kai_core::traits::{
    Agent, ApprovalDecision, StepOutcome, Tool, ToolApprovalPolicy, ToolContext,
};
use kai_core::ToolResultCache;
use tokio::sync::Mutex;

use crate::inbox::TaskInbox;
use crate::middleware::MiddlewarePipeline;

/// Default maximum consecutive reasoning turns permitted in [`OrchestrationEngine::run`].
pub const DEFAULT_MAX_TURNS: usize = 50;

/// Default maximum consecutive self-correction retries when tools fail.
pub const DEFAULT_MAX_CORRECTION_ATTEMPTS: usize = 2;

/// Central event-driven execution runtime coordinating an agent and its tool suite.
pub struct OrchestrationEngine {
    agent: Arc<Mutex<dyn Agent>>,
    inbox: Arc<TaskInbox>,
    tools: HashMap<String, Arc<dyn Tool>>,
    middleware: MiddlewarePipeline,
    event_bus: Option<EventBus>,
    steering: Option<GlobalSteeringReceiver>,
    working_dir: PathBuf,
    session_id: String,
    max_turns: usize,
    max_correction_attempts: usize,
    tool_failure_counts: HashMap<String, usize>,
    approval_policy: Option<Arc<dyn ToolApprovalPolicy>>,
    tool_cache: Option<Arc<ToolResultCache>>,
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
            middleware: MiddlewarePipeline::new(),
            event_bus: None,
            steering: None,
            working_dir: working_dir.into(),
            session_id: session_id.into(),
            max_turns: DEFAULT_MAX_TURNS,
            max_correction_attempts: DEFAULT_MAX_CORRECTION_ATTEMPTS,
            tool_failure_counts: HashMap::new(),
            approval_policy: None,
            tool_cache: None,
        }
    }

    /// Attaches an ordered [`MiddlewarePipeline`] to the engine.
    pub fn with_middleware(mut self, middleware: MiddlewarePipeline) -> Self {
        self.middleware = middleware;
        self
    }

    /// Configures a [`ToolApprovalPolicy`] for evaluating risk and authorization before tools run.
    pub fn with_approval_policy(mut self, policy: Arc<dyn ToolApprovalPolicy>) -> Self {
        self.approval_policy = Some(policy);
        self
    }

    /// Attaches a [`ToolResultCache`] for deterministic read-only tool memoization.
    pub fn with_tool_cache(mut self, cache: Arc<ToolResultCache>) -> Self {
        self.tool_cache = Some(cache);
        self
    }

    /// Configures maximum self-correction diagnostic retries for failed tool calls.
    pub fn with_max_correction_attempts(mut self, max_attempts: usize) -> Self {
        self.max_correction_attempts = max_attempts;
        self
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
        let mut incoming_messages = self.inbox.drain_all().await;

        let agent_id = {
            let guard = self.agent.lock().await;
            guard.id().to_string()
        };

        let step_ctx = self.build_tool_context(&agent_id);

        // Execute before_turn middleware hooks
        if let Err(err) = self
            .middleware
            .execute_before_turn(&mut incoming_messages, &step_ctx)
            .await
        {
            for msg in incoming_messages {
                let _ = self.inbox.enqueue(msg).await;
            }
            return Err(err);
        }

        // 3. Emit step started telemetry
        if let Some(bus) = &self.event_bus {
            let _ = bus.publish(Event::AgentStarted {
                agent_id: agent_id.clone(),
                task: "step".to_string(),
            });
        }

        // 4. Execute agent reasoning step
        let mut outcome = {
            let mut guard = self.agent.lock().await;
            guard.step(&incoming_messages).await?
        };

        // Execute after_turn middleware hooks
        self.middleware
            .execute_after_turn(&mut outcome, &step_ctx)
            .await?;

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

                        // 1. Evaluate ToolApprovalPolicy if configured
                        if let Some(policy) = &self.approval_policy {
                            match policy.evaluate(&tool_name, &call.arguments, &tool_ctx) {
                                ApprovalDecision::RequiresConfirmation { reason } => {
                                    if let Some(bus) = &self.event_bus {
                                        let _ = bus.publish(Event::Interrupted {
                                            agent_id: agent_id.clone(),
                                            reason: format!(
                                                "Tool '{tool_name}' requires confirmation: {reason}"
                                            ),
                                        });
                                    }
                                    return Ok(StepOutcome::Suspended {
                                        reason: format!(
                                            "Tool '{tool_name}' requires confirmation: {reason}"
                                        ),
                                    });
                                }
                                ApprovalDecision::Denied { reason } => {
                                    let denied_result = ToolResult::error(
                                        &tool_call_id,
                                        format!(
                                            "Policy denied execution of '{tool_name}': {reason}"
                                        ),
                                    );
                                    if let Some(bus) = &self.event_bus {
                                        let _ = bus.publish(Event::ToolCompleted {
                                            agent_id: agent_id.clone(),
                                            tool_result: denied_result.clone(),
                                        });
                                    }
                                    let response_msg = Message::tool_results(
                                        format!("resp_{tool_call_id}"),
                                        vec![denied_result],
                                    );
                                    self.inbox.enqueue(response_msg).await?;
                                    continue;
                                }
                                ApprovalDecision::Approved => {}
                            }
                        }

                        // 2. Check ToolResultCache for read-only tools
                        let cached_opt = if let (Some(cache), Some(tool)) =
                            (&self.tool_cache, self.tools.get(&tool_name))
                        {
                            if tool.is_read_only() {
                                cache.get(&tool_name, &call.arguments, &self.working_dir)
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        let mut tool_result = if let Some(mut cached) = cached_opt {
                            cached.tool_call_id = tool_call_id.clone();
                            cached
                        } else if let Some(tool) = self.tools.get(&tool_name) {
                            let executed =
                                match tool.execute(call.arguments.clone(), &tool_ctx).await {
                                    Ok(res) => {
                                        if res.is_error {
                                            let err = kai_core::error::ToolError::ExecutionFailed {
                                                name: tool_name.clone(),
                                                reason: res.output.clone(),
                                            };
                                            if let Some(recovered) = self
                                                .middleware
                                                .execute_on_tool_error(&tool_name, &err, &tool_ctx)
                                                .await?
                                            {
                                                recovered
                                            } else {
                                                res
                                            }
                                        } else {
                                            res
                                        }
                                    }
                                    Err(err) => {
                                        let fallback_err;
                                        let tool_err = match &err {
                                            KaiError::Tool(te) => te,
                                            other => {
                                                fallback_err =
                                                    kai_core::error::ToolError::ExecutionFailed {
                                                        name: tool_name.clone(),
                                                        reason: other.to_string(),
                                                    };
                                                &fallback_err
                                            }
                                        };
                                        if let Some(recovered) = self
                                            .middleware
                                            .execute_on_tool_error(&tool_name, tool_err, &tool_ctx)
                                            .await?
                                        {
                                            recovered
                                        } else {
                                            ToolResult::error(&tool_call_id, err.to_string())
                                        }
                                    }
                                };

                            // Memoize in cache if tool is read-only and succeeded
                            if !executed.is_error && tool.is_read_only() {
                                if let Some(cache) = &self.tool_cache {
                                    cache.insert(
                                        &tool_name,
                                        &call.arguments,
                                        &self.working_dir,
                                        executed.clone(),
                                    );
                                }
                            }

                            executed
                        } else {
                            ToolResult::error(
                                &tool_call_id,
                                format!("Tool not found in registry: '{tool_name}'"),
                            )
                        };

                        // Self-correction loop: append reflection diagnostic notice if error persisted
                        if tool_result.is_error {
                            let count = self
                                .tool_failure_counts
                                .entry(tool_name.clone())
                                .or_insert(0);
                            *count += 1;
                            if self.max_correction_attempts > 0 {
                                if *count <= self.max_correction_attempts {
                                    tool_result.output.push_str(&format!(
                                        "\n[Diagnostic: Tool '{}' produced an error (attempt {}/{}). Evaluate arguments and execute remediation]",
                                        tool_name, *count, self.max_correction_attempts
                                    ));
                                } else {
                                    tool_result.output.push_str(&format!(
                                        "\n[Diagnostic: Tool '{}' exceeded maximum self-correction attempts ({}/{}). Halt repeating call]",
                                        tool_name, *count, self.max_correction_attempts
                                    ));
                                }
                            }
                        } else {
                            self.tool_failure_counts.remove(&tool_name);
                        }

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
