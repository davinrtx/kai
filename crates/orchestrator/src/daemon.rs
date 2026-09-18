//! Long-running daemon supervisor and lifecycle harness for persistent agents.
//!
//! Provides [`DaemonSupervisor`] for background agent service loops, periodic heartbeat
//! tracking, health monitoring, and graceful shutdown coordination.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kai_core::error::{KaiError, OrchestratorError, Result};
use kai_core::message::current_timestamp_ms;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::engine::OrchestrationEngine;

/// Operating status of a daemonized agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonState {
    /// Initializing runtime components.
    Initializing,
    /// Actively polling and executing agent turns.
    Running,
    /// Suspended awaiting resumption signal.
    Paused,
    /// Undergoing graceful termination and resource flush.
    Stopping,
    /// Terminated cleanly.
    Stopped,
}

/// Long-running harness supervising an [`OrchestrationEngine`] as a background service.
pub struct DaemonSupervisor {
    engine: Arc<Mutex<OrchestrationEngine>>,
    state: Arc<RwLock<DaemonState>>,
    last_heartbeat_ms: Arc<AtomicU64>,
    heartbeat_interval: Duration,
    shutdown_tx: mpsc::Sender<()>,
    shutdown_rx: Mutex<mpsc::Receiver<()>>,
}

impl DaemonSupervisor {
    /// Constructs a new [`DaemonSupervisor`].
    pub fn new(engine: OrchestrationEngine, heartbeat_interval: Duration) -> Self {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let now = current_timestamp_ms();

        Self {
            engine: Arc::new(Mutex::new(engine)),
            state: Arc::new(RwLock::new(DaemonState::Initializing)),
            last_heartbeat_ms: Arc::new(AtomicU64::new(now)),
            heartbeat_interval,
            shutdown_tx,
            shutdown_rx: Mutex::new(shutdown_rx),
        }
    }

    /// Obtains the current operating state.
    pub async fn state(&self) -> DaemonState {
        *self.state.read().await
    }

    /// Retrieves the timestamp of the most recent heartbeat tick in milliseconds.
    pub fn last_heartbeat_ms(&self) -> u64 {
        self.last_heartbeat_ms.load(Ordering::SeqCst)
    }

    /// Requests graceful shutdown of the daemon service.
    pub async fn request_shutdown(&self) -> Result<()> {
        let _ = self.shutdown_tx.try_send(());
        Ok(())
    }

    /// Executes the primary supervision loop until shutdown is signaled or an unrecoverable error occurs.
    pub async fn run(&self) -> Result<()> {
        {
            let mut state_guard = self.state.write().await;
            *state_guard = DaemonState::Running;
        }

        let mut ticker = tokio::time::interval(self.heartbeat_interval);
        let mut shutdown_guard = self.shutdown_rx.lock().await;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.last_heartbeat_ms.store(current_timestamp_ms(), Ordering::SeqCst);

                    // Execute a step turn if work is pending
                    let mut engine_guard = self.engine.lock().await;
                    match engine_guard.step().await {
                        Ok(kai_core::traits::StepOutcome::Suspended { .. }) => {
                            let mut state_guard = self.state.write().await;
                            *state_guard = DaemonState::Paused;
                        }
                        Ok(_) => {
                            let mut state_guard = self.state.write().await;
                            if *state_guard == DaemonState::Paused {
                                *state_guard = DaemonState::Running;
                            }
                        }
                        Err(KaiError::Orchestrator(OrchestratorError::Interrupted { .. })) => {
                            // Interrupted by steering signal
                            break;
                        }
                        Err(err) => {
                            let mut state_guard = self.state.write().await;
                            *state_guard = DaemonState::Stopping;
                            return Err(err);
                        }
                    }
                }
                _ = shutdown_guard.recv() => {
                    // Graceful shutdown requested
                    break;
                }
            }
        }

        {
            let mut state_guard = self.state.write().await;
            *state_guard = DaemonState::Stopping;
        }

        {
            let mut state_guard = self.state.write().await;
            *state_guard = DaemonState::Stopped;
        }

        Ok(())
    }
}
