//! Session DAG Anti-Amnesia failure tombstones and reflection nodes.
//!
//! When an execution attempt or code modification fails and is rolled back, standard DAGs
//! suffer from "amnesia", causing LLMs to repeat identical mistakes.
//! [`TombstoneCoordinator`] captures structured failure diagnostics and negative constraints,
//! embedding them as persistent reflection nodes along rollback branch points.

use kai_core::error::Result;
use kai_core::message::{current_timestamp_ms, FailureTombstone, Message};
use kai_core::traits::{SessionNode, SessionStore};

/// Header tag identifying a session node as a failure tombstone reflection.
pub const TOMBSTONE_TAG: &str = "[FAILURE TOMBSTONE]";

/// Coordinator for managing failure tombstones and injecting negative constraints during rollbacks.
#[derive(Debug, Clone, Copy, Default)]
pub struct TombstoneCoordinator;

impl TombstoneCoordinator {
    /// Constructs a new [`TombstoneCoordinator`].
    pub fn new() -> Self {
        Self
    }

    /// Creates a [`SessionNode`] containing a failure tombstone reflection message.
    pub fn create_tombstone_node(parent_id: &str, tombstone: &FailureTombstone) -> SessionNode {
        let now = current_timestamp_ms();
        let node_id = format!("tombstone_{}_{now}", tombstone.failed_node_id);
        let serialized_json = serde_json::to_string(tombstone).unwrap_or_default();

        let content = format!(
            "{TOMBSTONE_TAG}\n{}\n```json\n{}\n```",
            tombstone.format_as_negative_prompt(),
            serialized_json
        );

        let message = Message::system(format!("msg_{node_id}"), content);
        SessionNode::with_parent(node_id, parent_id, message, now)
    }

    /// Performs a DAG rollback to `target_parent_id`, attaching a failure tombstone node
    /// at the fork to preserve diagnostic memory and prevent repeating the failure.
    pub async fn rollback_with_tombstone(
        store: &(dyn SessionStore + Send + Sync),
        target_parent_id: &str,
        tombstone: FailureTombstone,
    ) -> Result<SessionNode> {
        let node = Self::create_tombstone_node(target_parent_id, &tombstone);
        store.put_node(&node).await?;
        Ok(node)
    }

    /// Traverses the branch history ending at `leaf_node_id` and extracts all failure tombstones.
    pub async fn collect_tombstones(
        store: &(dyn SessionStore + Send + Sync),
        leaf_node_id: &str,
    ) -> Result<Vec<FailureTombstone>> {
        let history = store.get_branch_history(leaf_node_id).await?;
        let mut tombstones = Vec::new();

        for node in history {
            let text = node.message.text_content();
            if text.contains(TOMBSTONE_TAG) {
                if let Some(json_start) = text.find("```json\n") {
                    let json_payload = &text[json_start + 8..];
                    if let Some(json_end) = json_payload.find("\n```") {
                        let json_str = &json_payload[..json_end];
                        if let Ok(tombstone) = serde_json::from_str::<FailureTombstone>(json_str) {
                            tombstones.push(tombstone);
                            continue;
                        }
                    }
                }
            }
        }

        Ok(tombstones)
    }

    /// Formats a list of failure tombstones into a concatenated negative prompt section.
    pub fn format_negative_constraints(tombstones: &[FailureTombstone]) -> String {
        if tombstones.is_empty() {
            return String::new();
        }

        let mut out = String::from("### NEGATIVE CONSTRAINTS (Learned from previous failures):\n");
        for (idx, t) in tombstones.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] (Root cause: {})\n",
                idx + 1,
                t.trigger_action,
                t.root_cause_analysis
            ));
            for constraint in &t.negative_constraints {
                out.push_str(&format!("   - {constraint}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::MemorySessionStore;
    use kai_core::traits::SessionStore;

    #[tokio::test]
    async fn test_rollback_with_tombstone_and_collection() {
        let store = MemorySessionStore::new();

        // 1. Setup initial root node
        let root = SessionNode::root("root", Message::user("m1", "create feature"), 1000);
        store.put_node(&root).await.expect("put root");

        // 2. Setup failed step node
        let fail_step = SessionNode::with_parent(
            "step_failed",
            "root",
            Message::assistant("m2", "applied bad patch"),
            1001,
        );
        store.put_node(&fail_step).await.expect("put fail_step");

        // 3. Rollback to root with FailureTombstone
        let tombstone = FailureTombstone::new(
            "feat/patch",
            "step_failed",
            "modify parser",
            "patch applied with wrong lines",
            "unified diff line drift",
        )
        .with_negative_constraints(vec![
            "Never use strict line numbers; use fuzzy SEARCH/REPLACE blocks".to_string(),
        ]);

        let tombstone_node =
            TombstoneCoordinator::rollback_with_tombstone(&store, "root", tombstone)
                .await
                .expect("rollback");

        assert_eq!(tombstone_node.parent_ids, vec!["root"]);

        // 4. Collect tombstones along new branch
        let tombstones = TombstoneCoordinator::collect_tombstones(&store, &tombstone_node.id)
            .await
            .expect("collect");

        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].failed_node_id, "step_failed");
        assert!(tombstones[0].negative_constraints[0].contains("SEARCH/REPLACE"));

        let constraints = TombstoneCoordinator::format_negative_constraints(&tombstones);
        assert!(constraints.contains("NEGATIVE CONSTRAINTS"));
        assert!(constraints.contains("SEARCH/REPLACE"));
    }
}
