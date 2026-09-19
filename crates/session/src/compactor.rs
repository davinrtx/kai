//! Automatic DAG session history compaction and token reduction.
//!
//! Provides [`AutoCompactor`] for monitoring conversational turn depth and token
//! budgets, condensing older turns into checkpoint summary nodes without breaking
//! DAG continuity or corrupting historical branches.

use std::sync::Arc;

use kai_core::error::Result;
use kai_core::message::{current_timestamp_ms, Message};
use kai_core::traits::{ContextProcessor, SessionNode, SessionStore};

/// Default maximum turn depth threshold before auto-compaction triggers.
pub const DEFAULT_MAX_TURNS_THRESHOLD: usize = 20;

/// Default maximum token weight threshold before auto-compaction triggers.
pub const DEFAULT_MAX_TOKENS_THRESHOLD: usize = 8192;

/// Default number of recent conversational turns preserved uncompacted.
pub const DEFAULT_KEEP_RECENT_TURNS: usize = 5;

/// Automatic compactor for bounding session DAG memory and inference token overhead.
pub struct AutoCompactor {
    max_turns: usize,
    max_tokens: usize,
    keep_recent: usize,
    processor: Option<Arc<dyn ContextProcessor>>,
}

impl Default for AutoCompactor {
    fn default() -> Self {
        Self::new()
    }
}

impl AutoCompactor {
    /// Constructs a new [`AutoCompactor`] with standard default thresholds.
    pub fn new() -> Self {
        Self {
            max_turns: DEFAULT_MAX_TURNS_THRESHOLD,
            max_tokens: DEFAULT_MAX_TOKENS_THRESHOLD,
            keep_recent: DEFAULT_KEEP_RECENT_TURNS,
            processor: None,
        }
    }

    /// Configures the maximum allowed turn depth threshold.
    pub fn with_max_turns(mut self, max_turns: usize) -> Self {
        self.max_turns = max_turns.max(2);
        self
    }

    /// Configures the maximum allowed token budget threshold.
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens.max(256);
        self
    }

    /// Configures the number of recent conversational turns to preserve untouched.
    pub fn with_keep_recent(mut self, keep_recent: usize) -> Self {
        self.keep_recent = keep_recent.max(1);
        self
    }

    /// Attaches an optional [`ContextProcessor`] for intelligent token estimation and compression.
    pub fn with_context_processor(mut self, processor: Arc<dyn ContextProcessor>) -> Self {
        self.processor = Some(processor);
        self
    }

    /// Returns the maximum allowed turn depth threshold.
    pub fn max_turns(&self) -> usize {
        self.max_turns
    }

    /// Returns the maximum allowed token budget threshold.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Returns the count of recent turns preserved uncompacted.
    pub fn keep_recent(&self) -> usize {
        self.keep_recent
    }

    /// Evaluates whether the given linear history warrants compaction.
    pub fn should_compact(&self, history: &[SessionNode]) -> bool {
        if history.len() > self.max_turns {
            return true;
        }

        if let Some(proc) = &self.processor {
            let messages: Vec<Message> = history.iter().map(|n| n.message.clone()).collect();
            let estimated = proc.estimate_tokens(&messages);
            if estimated > self.max_tokens {
                return true;
            }
        }

        false
    }

    /// Performs DAG compaction on the linear branch terminating at `leaf_node_id`.
    ///
    /// If compaction is triggered, older turns are summarized into a new checkpoint node,
    /// and recent turns are re-chained on top of the compaction node.
    /// Returns [`Some(SessionNode)`] representing the new branch leaf, or [`None`] if not compacted.
    pub async fn compact_branch(
        &self,
        store: &(dyn SessionStore + Send + Sync),
        leaf_node_id: &str,
    ) -> Result<Option<SessionNode>> {
        let history = store.get_branch_history(leaf_node_id).await?;

        if !self.should_compact(&history) {
            return Ok(None);
        }

        let total_turns = history.len();
        if total_turns <= self.keep_recent {
            return Ok(None);
        }

        let split_idx = total_turns.saturating_sub(self.keep_recent);
        if split_idx == 0 {
            return Ok(None);
        }

        let older_nodes = &history[..split_idx];
        let recent_nodes = &history[split_idx..];

        // 1. Generate compacted summary text
        let compacted_summary = if let Some(proc) = &self.processor {
            let older_messages: Vec<Message> =
                older_nodes.iter().map(|n| n.message.clone()).collect();
            let target_tokens = self.max_tokens / 2;
            let compressed = proc.process(&older_messages, target_tokens).await?;
            let texts: Vec<String> = compressed.iter().map(|m| m.text_content()).collect();
            format!(
                "[Compacted history: {} turns condensed]\n{}",
                split_idx,
                texts.join("\n")
            )
        } else {
            let mut summary = format!("[Compacted history: {} turns condensed]\n", split_idx);
            for (idx, node) in older_nodes.iter().enumerate() {
                let role_str = match node.message.role {
                    kai_core::message::Role::System => "system",
                    kai_core::message::Role::User => "user",
                    kai_core::message::Role::Assistant => "assistant",
                    kai_core::message::Role::Tool => "tool",
                };
                summary.push_str(&format!(
                    "- Turn {}: {}: {}\n",
                    idx + 1,
                    role_str,
                    node.message.text_content()
                ));
            }
            summary
        };

        // Preserve all failure tombstones and negative constraints from compacted turns
        let mut tombstones = Vec::new();
        for node in older_nodes {
            let text = node.message.text_content();
            if text.contains(crate::tombstone::TOMBSTONE_TAG) {
                tombstones.push(text);
            }
        }

        let final_summary = if !tombstones.is_empty() {
            format!(
                "{compacted_summary}\n\n### PRESERVED FAILURE TOMBSTONES (Anti-Amnesia):\n{}",
                tombstones.join("\n\n")
            )
        } else {
            compacted_summary
        };

        let now = current_timestamp_ms();
        let compact_node_id = format!("compact_{now}_{split_idx}");
        let compact_msg = Message::system(format!("msg_{compact_node_id}"), final_summary);

        // 2. The compaction node becomes a root or links to parents of the first compacted node
        let compact_node = if !older_nodes[0].parent_ids.is_empty() {
            SessionNode::with_parents(
                compact_node_id.clone(),
                older_nodes[0].parent_ids.clone(),
                compact_msg,
                now,
            )
        } else {
            SessionNode::root(compact_node_id.clone(), compact_msg, now)
        };

        store.put_node(&compact_node).await?;

        // 3. Re-chain recent turns sequentially on top of the compaction node
        let mut last_parent_id = compact_node.id.clone();
        let mut final_leaf = compact_node;

        for (idx, original) in recent_nodes.iter().enumerate() {
            let new_node_id = format!("compact_relink_{now}_{idx}");
            let mut relinked_node = if original.parent_ids.len() > 1 {
                let mut parents = vec![last_parent_id.clone()];
                parents.extend(original.parent_ids.iter().skip(1).cloned());
                SessionNode::with_parents(
                    new_node_id,
                    parents,
                    original.message.clone(),
                    original.timestamp_ms,
                )
            } else {
                SessionNode::with_parent(
                    new_node_id,
                    &last_parent_id,
                    original.message.clone(),
                    original.timestamp_ms,
                )
            };

            if let Some(commit) = &original.git_commit {
                relinked_node = relinked_node.with_git_commit(commit.clone());
            }

            store.put_node(&relinked_node).await?;
            last_parent_id = relinked_node.id.clone();
            final_leaf = relinked_node;
        }

        Ok(Some(final_leaf))
    }
}
