//! In-memory Directed Acyclic Graph (DAG) session store.
//!
//! Provides [`MemorySessionStore`] implementing [`SessionStore`] for branching
//! conversational histories, cycle-safe node insertion, and fast parent/child traversals.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kai_core::error::{KaiError, Result, SessionError};
use kai_core::traits::{BoxFuture, SessionNode, SessionStore};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Serialized representation of an entire session DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGraphData {
    /// Unique identifier for this session.
    pub session_id: String,
    /// All nodes indexed by node identifier.
    pub nodes: HashMap<String, SessionNode>,
    /// Named branch pointers mapped to leaf node identifiers.
    pub branches: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct GraphState {
    nodes: HashMap<String, SessionNode>,
    children: HashMap<String, Vec<String>>,
    branches: HashMap<String, String>,
    commit_index: HashMap<String, String>,
    sessions: HashSet<String>,
}

/// In-memory thread-safe implementation of [`SessionStore`].
#[derive(Debug, Clone)]
pub struct MemorySessionStore {
    state: Arc<RwLock<GraphState>>,
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySessionStore {
    /// Constructs an empty [`MemorySessionStore`].
    pub fn new() -> Self {
        let mut state = GraphState::default();
        state.sessions.insert("default".to_string());
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Constructs a [`MemorySessionStore`] initialized with a specific session identifier.
    pub fn with_session(session_id: impl Into<String>) -> Self {
        let mut state = GraphState::default();
        state.sessions.insert(session_id.into());
        Self {
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Checks if a node would introduce a cycle before inserting.
    fn check_cycle(
        state: &GraphState,
        node_id: &str,
        parent_ids: &[String],
    ) -> std::result::Result<(), SessionError> {
        let mut queue: Vec<&str> = parent_ids.iter().map(String::as_str).collect();
        let mut visited = HashSet::new();

        while let Some(current_id) = queue.pop() {
            if current_id == node_id {
                return Err(SessionError::CycleDetected {
                    node_id: node_id.to_string(),
                });
            }

            if visited.insert(current_id) {
                if let Some(parent_node) = state.nodes.get(current_id) {
                    for next_parent in &parent_node.parent_ids {
                        queue.push(next_parent.as_str());
                    }
                }
            }
        }

        Ok(())
    }

    /// Exports the current in-memory graph state for persistence.
    pub async fn export_data(&self, session_id: &str) -> SessionGraphData {
        let guard = self.state.read().await;
        SessionGraphData {
            session_id: session_id.to_string(),
            nodes: guard.nodes.clone(),
            branches: guard.branches.clone(),
        }
    }

    /// Internal helper applying imported graph state to mutable state.
    fn apply_import(state: &mut GraphState, data: SessionGraphData) {
        state.sessions.insert(data.session_id);

        for (id, node) in data.nodes {
            if let Some(commit) = &node.git_commit {
                state.commit_index.insert(commit.clone(), id.clone());
            }
            for parent_id in &node.parent_ids {
                let children = state.children.entry(parent_id.clone()).or_default();
                if !children.contains(&id) {
                    children.push(id.clone());
                }
            }
            state.nodes.insert(id, node);
        }

        for (branch, head) in data.branches {
            state.branches.insert(branch, head);
        }
    }

    /// Synchronously imports graph state into this in-memory store without runtime blocking.
    pub fn import_data_sync(&self, data: SessionGraphData) -> Result<()> {
        let mut guard = self.state.try_write().map_err(|_| {
            KaiError::Internal(kai_core::error::InternalError::new(
                "Failed to acquire session state lock for synchronous import",
            ))
        })?;
        Self::apply_import(&mut guard, data);
        Ok(())
    }

    /// Imports graph state into this in-memory store.
    pub async fn import_data(&self, data: SessionGraphData) -> Result<()> {
        let mut guard = self.state.write().await;
        Self::apply_import(&mut guard, data);
        Ok(())
    }

    /// Returns the total count of nodes stored in the DAG.
    pub async fn node_count(&self) -> usize {
        let guard = self.state.read().await;
        guard.nodes.len()
    }
}

impl SessionStore for MemorySessionStore {
    fn get_node<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Option<SessionNode>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            Ok(guard.nodes.get(node_id).cloned())
        })
    }

    fn put_node<'a>(&'a self, node: &'a SessionNode) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // 1. Validate internal node invariants (non-empty ID, no self-parenting)
            node.validate().map_err(KaiError::Session)?;

            let mut guard = self.state.write().await;

            // 2. Validate parents exist if not a root node
            for parent_id in &node.parent_ids {
                if !guard.nodes.contains_key(parent_id) {
                    return Err(KaiError::Session(SessionError::NodeNotFound {
                        node_id: parent_id.clone(),
                    }));
                }
            }

            // 3. Detect cycles in the DAG
            Self::check_cycle(&guard, &node.id, &node.parent_ids).map_err(KaiError::Session)?;

            // 4. Update children index, cleaning previous parent associations if updated
            let (stale_parents, old_commit) = if let Some(existing) = guard.nodes.get(&node.id) {
                let stale: Vec<String> = existing
                    .parent_ids
                    .iter()
                    .filter(|p| !node.parent_ids.contains(p))
                    .cloned()
                    .collect();
                let commit = existing.git_commit.clone();
                (stale, commit)
            } else {
                (Vec::new(), None)
            };

            for old_parent in &stale_parents {
                if let Some(children) = guard.children.get_mut(old_parent) {
                    children.retain(|c| c != &node.id);
                }
            }
            if let Some(old_c) = &old_commit {
                if Some(old_c) != node.git_commit.as_ref() {
                    guard.commit_index.remove(old_c);
                }
            }

            for parent_id in &node.parent_ids {
                let children = guard.children.entry(parent_id.clone()).or_default();
                if !children.contains(&node.id) {
                    children.push(node.id.clone());
                }
            }

            // 5. Update Git commit index if present
            if let Some(commit) = &node.git_commit {
                guard.commit_index.insert(commit.clone(), node.id.clone());
            }

            // 6. Insert node
            guard.nodes.insert(node.id.clone(), node.clone());

            Ok(())
        })
    }

    fn get_children<'a>(&'a self, node_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            if let Some(child_ids) = guard.children.get(node_id) {
                let mut children: Vec<SessionNode> = child_ids
                    .iter()
                    .filter_map(|id| guard.nodes.get(id).cloned())
                    .collect();
                children.sort_by(|a, b| a.id.cmp(&b.id));
                Ok(children)
            } else {
                Ok(Vec::new())
            }
        })
    }

    fn get_branch_history<'a>(
        &'a self,
        leaf_node_id: &'a str,
    ) -> BoxFuture<'a, Result<Vec<SessionNode>>> {
        Box::pin(async move {
            let guard = self.state.read().await;

            let mut history = Vec::new();
            let mut current_id = Some(leaf_node_id.to_string());
            let mut visited = HashSet::new();

            while let Some(id) = current_id {
                if !visited.insert(id.clone()) {
                    return Err(KaiError::Session(SessionError::CycleDetected {
                        node_id: id,
                    }));
                }

                if let Some(node) = guard.nodes.get(&id) {
                    current_id = node.primary_parent().map(String::from);
                    history.push(node.clone());
                } else {
                    return Err(KaiError::Session(SessionError::NodeNotFound {
                        node_id: id,
                    }));
                }
            }

            history.reverse();
            Ok(history)
        })
    }

    fn set_head<'a>(&'a self, branch_name: &'a str, node_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            if !guard.nodes.contains_key(node_id) {
                return Err(KaiError::Session(SessionError::NodeNotFound {
                    node_id: node_id.to_string(),
                }));
            }
            guard
                .branches
                .insert(branch_name.to_string(), node_id.to_string());
            Ok(())
        })
    }

    fn get_head<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<Option<String>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            Ok(guard.branches.get(branch_name).cloned())
        })
    }

    fn list_branches<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            let mut list: Vec<String> = guard.branches.keys().cloned().collect();
            list.sort();
            Ok(list)
        })
    }

    fn prune_branch<'a>(&'a self, branch_name: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if branch_name == "main" || branch_name == "default" {
                return Err(KaiError::Session(SessionError::CorruptedState {
                    reason: format!("Cannot prune default branch '{branch_name}'"),
                }));
            }
            let mut guard = self.state.write().await;
            if guard.branches.remove(branch_name).is_none() {
                return Err(KaiError::Session(SessionError::NotFound {
                    session_id: format!("Branch '{branch_name}' does not exist"),
                }));
            }
            Ok(())
        })
    }

    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<String>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            let mut list: Vec<String> = guard.sessions.iter().cloned().collect();
            list.sort();
            Ok(list)
        })
    }

    fn get_node_by_commit<'a>(
        &'a self,
        commit_hash: &'a str,
    ) -> BoxFuture<'a, Result<Option<SessionNode>>> {
        Box::pin(async move {
            let guard = self.state.read().await;
            if let Some(node_id) = guard.commit_index.get(commit_hash) {
                Ok(guard.nodes.get(node_id).cloned())
            } else {
                Ok(None)
            }
        })
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut guard = self.state.write().await;
            let removed = guard.sessions.remove(session_id);
            if guard.sessions.is_empty() {
                guard.nodes.clear();
                guard.children.clear();
                guard.branches.clear();
                guard.commit_index.clear();
            }
            if !removed {
                return Err(KaiError::Session(SessionError::NotFound {
                    session_id: session_id.to_string(),
                }));
            }
            Ok(())
        })
    }
}
