//! # kai-orchestrator
//!
//! Multi-agent task dispatcher, event-driven orchestration engine, and daemon supervisor.
//!
//! Provides the execution coordination layer for autonomous agents:
//! - [`TaskInbox`]: Bounded asynchronous message queues backed by `tokio::sync::mpsc`.
//! - [`SubAgentDispatcher`]: Supervised sub-agent fleet management with fault isolation.
//! - [`OrchestrationEngine`]: Core event-driven loop integrating agents, tools, steering, and events.
//! - [`DaemonSupervisor`]: Long-running background service harness with heartbeat tracking.

pub mod daemon;
pub mod dispatcher;
pub mod engine;
pub mod inbox;

pub use daemon::{DaemonState, DaemonSupervisor};
pub use dispatcher::{
    SubAgentDispatcher, SubAgentInfo, SubAgentStatus, DEFAULT_MAX_CONCURRENT_SUBAGENTS,
};
pub use engine::{OrchestrationEngine, DEFAULT_MAX_TURNS};
pub use inbox::{InboxReceiver, InboxSender, TaskInbox, DEFAULT_INBOX_CAPACITY};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestrator_defaults() {
        assert_eq!(DEFAULT_INBOX_CAPACITY, 128);
        assert_eq!(DEFAULT_MAX_CONCURRENT_SUBAGENTS, 16);
        assert_eq!(DEFAULT_MAX_TURNS, 50);
    }
}
