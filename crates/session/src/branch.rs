//! High-level branch pointer lifecycle, checkout, fork, and merge coordinator.
//!
//! Provides [`BranchManager`] wrapping any [`SessionStore`] to manage named
//! branch heads, handle merges with multi-parent DAG nodes, and coordinate active context.

use std::sync::Arc;

use kai_core::error::{KaiError, Result, SessionError};
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::{SessionNode, SessionStore};
use tokio::sync::RwLock;

/// Default branch name for root session progression.
pub const DEFAULT_BRANCH_NAME: &str = "main";

/// Coordinator for branch pointers, switching, forking, and merging in the session DAG.
pub struct BranchManager {
    store: Arc<dyn SessionStore>,
    active_branch: RwLock<String>,
}

impl BranchManager {
    /// Constructs a new [`BranchManager`] attached to the given [`SessionStore`].
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self {
            store,
            active_branch: RwLock::new(DEFAULT_BRANCH_NAME.to_string()),
        }
    }

    /// Constructs a [`BranchManager`] with an initial active branch name.
    pub fn with_active_branch(
        store: Arc<dyn SessionStore>,
        branch_name: impl Into<String>,
    ) -> Self {
        Self {
            store,
            active_branch: RwLock::new(branch_name.into()),
        }
    }

    /// Returns the currently active branch name.
    pub async fn active_branch(&self) -> String {
        self.active_branch.read().await.clone()
    }

    /// Retrieves the leaf node identifier currently pointed to by the active branch.
    pub async fn active_head(&self) -> Result<Option<String>> {
        let branch = self.active_branch.read().await;
        self.store.get_head(&branch).await
    }

    /// Creates a new branch pointing to the designated node identifier.
    pub async fn create_branch(&self, branch_name: &str, node_id: &str) -> Result<()> {
        if branch_name.trim().is_empty() {
            return Err(KaiError::Session(SessionError::CorruptedState {
                reason: "Branch name cannot be empty".to_string(),
            }));
        }

        if self.store.get_head(branch_name).await?.is_some() {
            return Err(KaiError::Session(SessionError::CorruptedState {
                reason: format!("Branch '{branch_name}' already exists"),
            }));
        }

        // Verify the target node exists in the DAG
        if self.store.get_node(node_id).await?.is_none() {
            return Err(KaiError::Session(SessionError::NodeNotFound {
                node_id: node_id.to_string(),
            }));
        }

        self.store.set_head(branch_name, node_id).await
    }

    /// Switches the active branch pointer to an existing branch.
    pub async fn switch_branch(&self, branch_name: &str) -> Result<()> {
        if branch_name.trim().is_empty() {
            return Err(KaiError::Session(SessionError::CorruptedState {
                reason: "Branch name cannot be empty".to_string(),
            }));
        }

        let head = self.store.get_head(branch_name).await?;
        if head.is_none() {
            return Err(KaiError::Session(SessionError::NotFound {
                session_id: format!("Branch '{branch_name}' does not exist"),
            }));
        }

        let mut guard = self.active_branch.write().await;
        *guard = branch_name.to_string();
        Ok(())
    }

    /// Forks a new branch from an existing source branch's head.
    pub async fn fork_branch(&self, source_branch: &str, target_branch: &str) -> Result<()> {
        let head = self.store.get_head(source_branch).await?;
        if let Some(head_node_id) = head {
            self.create_branch(target_branch, &head_node_id).await
        } else {
            Err(KaiError::Session(SessionError::NotFound {
                session_id: format!("Source branch '{source_branch}' has no head node"),
            }))
        }
    }

    /// Merges `source_branch` into `target_branch`, creating a merge node with both parents.
    pub async fn merge_branches(
        &self,
        source_branch: &str,
        target_branch: &str,
        merge_message: Message,
    ) -> Result<SessionNode> {
        let source_head = self.store.get_head(source_branch).await?.ok_or_else(|| {
            KaiError::Session(SessionError::NotFound {
                session_id: format!("Source branch '{source_branch}' not found"),
            })
        })?;

        let target_head = self.store.get_head(target_branch).await?.ok_or_else(|| {
            KaiError::Session(SessionError::NotFound {
                session_id: format!("Target branch '{target_branch}' not found"),
            })
        })?;

        // Fast-forward: if heads are identical, no merge node needed
        if source_head == target_head {
            if let Some(node) = self.store.get_node(&target_head).await? {
                return Ok(node);
            }
        }

        let merge_node_id = format!("merge_{}_{}", target_head, source_head);
        let now = current_timestamp_ms();

        let merge_node = SessionNode::with_parents(
            merge_node_id,
            vec![target_head, source_head],
            merge_message,
            now,
        );

        self.store.put_node(&merge_node).await?;
        self.store.set_head(target_branch, &merge_node.id).await?;

        Ok(merge_node)
    }

    /// Lists all registered branch names.
    pub async fn list_branches(&self) -> Result<Vec<String>> {
        self.store.list_branches().await
    }

    /// Prunes an inactive branch. Rejects pruning the currently active branch.
    pub async fn prune_branch(&self, branch_name: &str) -> Result<()> {
        let active = self.active_branch.read().await;
        if branch_name == *active {
            return Err(KaiError::Session(SessionError::CorruptedState {
                reason: format!("Cannot prune currently active branch '{branch_name}'"),
            }));
        }

        self.store.prune_branch(branch_name).await
    }

    /// Associates a Git commit SHA with the current active branch head node.
    pub async fn attach_git_commit(&self, branch_name: &str, commit_sha: &str) -> Result<()> {
        let head_id = self.store.get_head(branch_name).await?.ok_or_else(|| {
            KaiError::Session(SessionError::NotFound {
                session_id: format!("Branch '{branch_name}' has no head node"),
            })
        })?;

        if let Some(mut node) = self.store.get_node(&head_id).await? {
            node = node.with_git_commit(commit_sha);
            self.store.put_node(&node).await?;
            Ok(())
        } else {
            Err(KaiError::Session(SessionError::NodeNotFound {
                node_id: head_id,
            }))
        }
    }

    /// Compacts the specified branch using the given [`AutoCompactor`].
    /// If compaction occurs, updates the branch head pointer to the newly created leaf.
    pub async fn compact_branch(
        &self,
        branch_name: &str,
        compactor: &crate::compactor::AutoCompactor,
    ) -> Result<Option<SessionNode>> {
        let head = self.store.get_head(branch_name).await?.ok_or_else(|| {
            KaiError::Session(SessionError::NotFound {
                session_id: format!("Branch '{branch_name}' has no head node"),
            })
        })?;

        if let Some(new_leaf) = compactor.compact_branch(self.store.as_ref(), &head).await? {
            self.store.set_head(branch_name, &new_leaf.id).await?;
            Ok(Some(new_leaf))
        } else {
            Ok(None)
        }
    }

    /// Compacts the currently active branch using the given [`AutoCompactor`].
    pub async fn compact_active_branch(
        &self,
        compactor: &crate::compactor::AutoCompactor,
    ) -> Result<Option<SessionNode>> {
        let active = self.active_branch().await;
        self.compact_branch(&active, compactor).await
    }

    /// Appends a new conversational turn to the specified branch, creating either a root node
    /// (if the branch has no head) or a child node of the current head, and advances the head pointer.
    pub async fn append_to_branch(
        &self,
        branch_name: &str,
        node_id: impl Into<String>,
        message: Message,
    ) -> Result<SessionNode> {
        let node_id = node_id.into();
        let head = self.store.get_head(branch_name).await?;
        let now = current_timestamp_ms();

        let node = if let Some(parent_id) = head {
            SessionNode::with_parent(node_id.clone(), parent_id, message, now)
        } else {
            SessionNode::root(node_id.clone(), message, now)
        };

        self.store.put_node(&node).await?;
        self.store.set_head(branch_name, &node.id).await?;
        Ok(node)
    }

    /// Appends a new conversational turn to the currently active branch, advancing the head pointer.
    pub async fn append_turn(
        &self,
        node_id: impl Into<String>,
        message: Message,
    ) -> Result<SessionNode> {
        let active = self.active_branch().await;
        self.append_to_branch(&active, node_id, message).await
    }

    /// Retrieves the linear conversational history of the specified branch.
    pub async fn branch_history(&self, branch_name: &str) -> Result<Vec<SessionNode>> {
        let head = self.store.get_head(branch_name).await?;
        if let Some(head_id) = head {
            self.store.get_branch_history(&head_id).await
        } else {
            Ok(Vec::new())
        }
    }

    /// Retrieves the linear conversational history of the currently active branch.
    pub async fn active_history(&self) -> Result<Vec<SessionNode>> {
        let active = self.active_branch().await;
        self.branch_history(&active).await
    }
}
