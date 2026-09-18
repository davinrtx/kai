//! Sub-agent fleet management, supervised dispatching, and fault isolation.
//!
//! Provides [`SubAgentDispatcher`] for registering, scheduling, and executing ephemeral
//! sub-agents in supervised tasks with strict failure containment and concurrency limits.

use std::collections::HashMap;
use std::sync::Arc;

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::{Agent, StepOutcome};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, Semaphore};

/// Maximum simultaneous sub-agent tasks permitted to execute concurrently.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 16;

/// Lifecycle state of an ephemeral sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubAgentStatus {
    /// Agent is registered and waiting for task assignment.
    Idle,
    /// Agent is currently executing a reasoning turn.
    Running,
    /// Agent concluded execution successfully.
    Completed,
    /// Agent terminated with an error or panic.
    Failed(String),
}

/// Metadata describing a registered sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentInfo {
    /// Unique instance identifier.
    pub id: String,
    /// Functional agent name/role.
    pub name: String,
    /// Current lifecycle status.
    pub status: SubAgentStatus,
    /// Timestamp of initial registration in milliseconds since UNIX epoch.
    pub registered_at_ms: u64,
    /// Timestamp of most recent activity.
    pub last_active_ms: u64,
}

struct AgentEntry {
    info: SubAgentInfo,
    instance: Arc<Mutex<dyn Agent>>,
}

/// Supervised dispatcher coordinating multi-agent task distribution and fault isolation.
#[derive(Clone)]
pub struct SubAgentDispatcher {
    agents: Arc<RwLock<HashMap<String, AgentEntry>>>,
    concurrency_semaphore: Arc<Semaphore>,
}

impl Default for SubAgentDispatcher {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT_SUBAGENTS)
    }
}

impl SubAgentDispatcher {
    /// Constructs a new [`SubAgentDispatcher`] with the designated concurrency limit.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            concurrency_semaphore: Arc::new(Semaphore::new(max_concurrent.max(1))),
        }
    }

    /// Registers a new agent into the dispatcher fleet.
    pub async fn register_agent(&self, agent: Arc<Mutex<dyn Agent>>) -> Result<String> {
        let (id, name) = {
            let guard = agent.lock().await;
            (guard.id().to_string(), guard.name().to_string())
        };

        let now = current_timestamp_ms();
        let info = SubAgentInfo {
            id: id.clone(),
            name,
            status: SubAgentStatus::Idle,
            registered_at_ms: now,
            last_active_ms: now,
        };

        let mut guard = self.agents.write().await;
        guard.insert(
            id.clone(),
            AgentEntry {
                info,
                instance: agent,
            },
        );

        Ok(id)
    }

    /// Removes an agent from the fleet by ID.
    pub async fn unregister_agent(&self, agent_id: &str) -> Result<()> {
        let mut guard = self.agents.write().await;
        if let Some(entry) = guard.get(agent_id) {
            if entry.info.status == SubAgentStatus::Running {
                return Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id.to_string(),
                    reason: format!("Cannot unregister sub-agent '{agent_id}' while it is running"),
                }));
            }
            guard.remove(agent_id);
            Ok(())
        } else {
            Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                agent_id: agent_id.to_string(),
                reason: "Sub-agent not found".to_string(),
            }))
        }
    }

    /// Queries the total number of registered sub-agents.
    pub async fn agent_count(&self) -> usize {
        let guard = self.agents.read().await;
        guard.len()
    }

    /// Retrieves status metadata for all registered sub-agents.
    pub async fn list_agents(&self) -> Vec<SubAgentInfo> {
        let guard = self.agents.read().await;
        let mut list: Vec<SubAgentInfo> = guard.values().map(|e| e.info.clone()).collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Retrieves status metadata for a specific sub-agent.
    pub async fn get_agent_info(&self, agent_id: &str) -> Option<SubAgentInfo> {
        let guard = self.agents.read().await;
        guard.get(agent_id).map(|e| e.info.clone())
    }

    /// Dispatches an execution turn to a registered sub-agent with supervised fault isolation.
    ///
    /// The agent step is executed within a supervised asynchronous task with a concurrency permit.
    /// Any panics or errors are captured and isolated, preventing corruption of the orchestrator.
    pub async fn dispatch(&self, agent_id: &str, inbox: Vec<Message>) -> Result<StepOutcome> {
        // 1. Acquire concurrency permit to bound simultaneous worker threads across the fleet
        let permit = self
            .concurrency_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id.to_string(),
                    reason: "Dispatcher concurrency semaphore closed".to_string(),
                })
            })?;

        // 2. Atomically verify existence and prevent concurrent re-entrant execution on same agent
        let agent_instance = {
            let mut guard = self.agents.write().await;
            if let Some(entry) = guard.get_mut(agent_id) {
                if entry.info.status == SubAgentStatus::Running {
                    return Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                        agent_id: agent_id.to_string(),
                        reason: format!("Sub-agent '{agent_id}' is already executing a turn"),
                    }));
                }
                entry.info.status = SubAgentStatus::Running;
                entry.info.last_active_ms = current_timestamp_ms();
                entry.instance.clone()
            } else {
                return Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id.to_string(),
                    reason: "Sub-agent not found".to_string(),
                }));
            }
        };

        let agent_id_owned = agent_id.to_string();

        // 3. Supervised task execution: catches panics, isolates errors, and holds permit via RAII
        let join_result = tokio::spawn(async move {
            let _permit = permit;
            let mut guard = agent_instance.lock().await;
            guard.step(&inbox).await
        })
        .await;

        let now = current_timestamp_ms();

        match join_result {
            Ok(Ok(outcome)) => {
                let mut guard = self.agents.write().await;
                if let Some(entry) = guard.get_mut(&agent_id_owned) {
                    entry.info.status = if outcome.is_completed() {
                        SubAgentStatus::Completed
                    } else {
                        SubAgentStatus::Idle
                    };
                    entry.info.last_active_ms = now;
                }
                Ok(outcome)
            }
            Ok(Err(err)) => {
                let err_msg = err.to_string();
                let mut guard = self.agents.write().await;
                if let Some(entry) = guard.get_mut(&agent_id_owned) {
                    entry.info.status = SubAgentStatus::Failed(err_msg.clone());
                    entry.info.last_active_ms = now;
                }
                Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id_owned,
                    reason: err_msg,
                }))
            }
            Err(panic_err) => {
                let panic_msg = format!("Sub-agent task panicked: {panic_err}");
                let mut guard = self.agents.write().await;
                if let Some(entry) = guard.get_mut(&agent_id_owned) {
                    entry.info.status = SubAgentStatus::Failed(panic_msg.clone());
                    entry.info.last_active_ms = now;
                }
                Err(KaiError::Orchestrator(OrchestratorError::SubAgentFailed {
                    agent_id: agent_id_owned,
                    reason: panic_msg,
                }))
            }
        }
    }
}
